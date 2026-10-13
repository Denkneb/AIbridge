//! Task-bound read-only review. Saved hashes are never presented as saved bytes.
use crate::projects::ProjectService;
use bridge_domain::TaskId;
use bridge_git::{
    RepositorySnapshot,
    diff::{FILE_LIMIT, commit_file, text_patch},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    ffi::OsStr,
    fs,
    io::Read,
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};
type Result<T> = std::result::Result<T, &'static str>;
struct Context {
    root: PathBuf,
    baseline: RepositorySnapshot,
    parent: Option<PathBuf>,
    repositories: Vec<Value>,
    mode: String,
}
fn safe_path(path: &str) -> Result<()> {
    if path.is_empty()
        || path.len() > 4096
        || path.contains(['\0', '\\'])
        || Path::new(path).is_absolute()
        || path
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == ".." || p.eq_ignore_ascii_case(".git"))
    {
        return Err("Недопустимый путь файла");
    }
    Ok(())
}
fn read_file(root: &Path, path: &str) -> Result<Option<Option<bridge_git::objects::CommitObject>>> {
    safe_path(path)?;
    let mut cursor = root.to_path_buf();
    let parts = path.split('/').collect::<Vec<_>>();
    for part in &parts[..parts.len() - 1] {
        cursor.push(part);
        match fs::symlink_metadata(&cursor) {
            Ok(m) if m.is_dir() && !m.file_type().is_symlink() => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            _ => return Err("Каталог файла недоступен или заменён ссылкой"),
        }
    }
    let target = root.join(path);
    let meta = match fs::symlink_metadata(&target) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        _ => return Err("Не удалось прочитать файл"),
    };
    let (mode, data) = if meta.file_type().is_symlink() {
        (
            0o120000,
            fs::read_link(target)
                .map_err(|_| "Не удалось прочитать ссылку")?
                .as_os_str()
                .as_bytes()
                .to_vec(),
        )
    } else {
        if !meta.is_file() {
            return Err("Неподдерживаемый тип файла");
        }
        if meta.len() > FILE_LIMIT as u64 {
            return Ok(Some(None));
        }
        let mut file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
            .open(target)
            .map_err(|_| "Не удалось прочитать файл")?;
        if !file
            .metadata()
            .map_err(|_| "Не удалось прочитать файл")?
            .is_file()
        {
            return Err("Тип файла изменился");
        }
        let mut data = vec![];
        (&mut file)
            .take(FILE_LIMIT as u64 + 1)
            .read_to_end(&mut data)
            .map_err(|_| "Не удалось прочитать файл")?;
        if data.len() > FILE_LIMIT {
            return Ok(Some(None));
        }
        (
            if meta.mode() & 0o111 != 0 {
                0o100755
            } else {
                0o100644
            },
            data,
        )
    };
    Ok(Some(Some(bridge_git::objects::CommitObject { mode, data })))
}
fn digest(object: &bridge_git::objects::CommitObject) -> String {
    // Match the snapshot's type-prefixed digest, including executable bits.
    bridge_git::diff::file_digest(object.mode, &object.data)
}
fn revision(snapshot: &RepositorySnapshot) -> String {
    format!(
        "{}:{}:{}",
        snapshot.head().map_or("", |h| h.as_str()),
        snapshot.index_fingerprint().to_hex(),
        snapshot.worktree_fingerprint().to_hex()
    )
}
impl ProjectService {
    fn diff_context(&self, project: &str, task: &str, repository: u32) -> Result<Context> {
        let (entry, layout) = self.project(project)?;
        let id: TaskId = task.parse().map_err(|_| "Некорректный ID задачи")?;
        let storage = layout
            .open_readonly()
            .map_err(|_| "Состояние проекта недоступно")?;
        let task = storage
            .get_task(id)
            .map_err(|_| "Состояние задачи недоступно")?
            .ok_or("Задача не найдена")?;
        if task.project_id != *entry.id() || Path::new(&task.workspace) != entry.workspace() {
            return Err("Задача относится к другому проекту");
        }
        let mode: String = storage
            .connection()
            .query_row(
                "SELECT execution_mode FROM tasks WHERE task_id=?1",
                [id.to_string()],
                |r| r.get(0),
            )
            .map_err(|_| "Режим задачи недоступен")?;
        let mut parent = None;
        let (root, baseline) = if mode == "worktree" {
            let record = storage
                .get_worktree(id, entry.id())
                .map_err(|_| "Рабочий каталог задачи недоступен")?
                .ok_or("Рабочий каталог задачи отсутствует")?;
            if record.status != bridge_storage::WorktreeStatus::Created
                || record.base_head != task.base_head
            {
                return Err("Рабочий каталог задачи больше недоступен");
            }
            let binding = bridge_git::checkout::probe_checkout(
                entry.workspace(),
                &layout.project_dir(),
                id,
                Path::new(&record.path),
                None,
            )
            .map_err(|_| "Принадлежность рабочего каталога не подтверждена")?;
            if record.runtime_dir.as_deref().map(Path::new)
                != Some(binding.paths.runtime_dir.as_path())
            {
                return Err("Принадлежность рабочего каталога не подтверждена");
            }
            if let Some(parent_id) = task
                .snapshot
                .as_ref()
                .and_then(|s| s["automation_parent"].as_str())
            {
                let parent_id: TaskId =
                    parent_id.parse().map_err(|_| "Исходный снимок повреждён")?;
                if let Some(record) = storage
                    .get_worktree(parent_id, entry.id())
                    .map_err(|_| "Исходный снимок недоступен")?
                {
                    parent = bridge_git::checkout::probe_checkout(
                        entry.workspace(),
                        &layout.project_dir(),
                        parent_id,
                        Path::new(&record.path),
                        None,
                    )
                    .ok()
                    .map(|b| b.paths.checkout);
                }
            }
            (
                binding.paths.checkout,
                serde_json::from_str::<Value>(
                    record
                        .baseline_json
                        .as_deref()
                        .ok_or("Исходный снимок отсутствует")?,
                )
                .map_err(|_| "Исходный снимок повреждён")?,
            )
        } else {
            (
                entry.workspace().to_path_buf(),
                task.snapshot
                    .clone()
                    .ok_or("Исходный снимок задачи отсутствует")?,
            )
        };
        let mut sources = vec![(root, baseline.clone(), "Основной репозиторий".to_string())];
        if mode == "direct"
            && let Some(externals) = baseline.get("external_repositories")
        {
            let externals = externals
                .as_array()
                .filter(|v| v.len() <= 63)
                .ok_or("Исходный снимок повреждён")?;
            for (index, external) in externals.iter().enumerate() {
                sources.push((
                    PathBuf::from(
                        external["root"]
                            .as_str()
                            .ok_or("Исходный снимок повреждён")?,
                    ),
                    external.clone(),
                    format!("Внешний репозиторий {}", index + 1),
                ));
            }
        }
        let repositories = sources
            .iter()
            .enumerate()
            .map(|(id, (_, _, label))| json!({"id":id,"label":label}))
            .collect();
        let (root, baseline, _) = sources
            .into_iter()
            .nth(repository as usize)
            .ok_or("Репозиторий задачи не найден")?;
        if fs::canonicalize(&root).map_err(|_| "Репозиторий недоступен")? != root
            || bridge_git::checkout::main_common_dir(&root).is_err()
        {
            return Err("Репозиторий недоступен или заменён");
        }
        let baseline =
            RepositorySnapshot::from_json(&baseline).map_err(|_| "Исходный снимок повреждён")?;
        if repository == 0 && baseline.head().map(|head| head.as_str()) != task.base_head.as_deref()
        {
            return Err("Исходный снимок не соответствует задаче");
        }
        Ok(Context {
            root,
            baseline,
            parent,
            repositories,
            mode,
        })
    }
    pub fn project_diff(&self, project: &str, scope: &str) -> Result<Value> {
        self.project_diff_at(project, scope, None)
    }
    pub fn project_diff_at(
        &self,
        project: &str,
        scope: &str,
        checkout: Option<&crate::ConsoleCheckout>,
    ) -> Result<Value> {
        let _activity = self.activity_guard()?;
        check_scope(scope)?;
        let (entry, _) = self.project(project)?;
        if checkout.is_none()
            && let Some(remote) = entry.remote_execution()
        {
            return bridge_automation::remote::rpc(
                remote,
                &json!({"op":"project_diff","scope":scope}),
            )
            .map_err(|_| "Diff второго ПК недоступен; обновите agent-bridge и проверьте SSH");
        }
        let selected = checkout
            .map(|checkout| self.resolve_console_checkout(project, checkout))
            .transpose()?;
        let root = selected.as_deref().unwrap_or(entry.workspace());
        let current = bridge_git::take_snapshot(root).map_err(|_| "Git проекта недоступен")?;
        let changes =
            bridge_git::diff::project_changes(root, scope, current.head().map(|h| h.as_str()))
                .map_err(|_| "Не удалось прочитать изменения Git")?;
        let mut files = vec![];
        for (path, change) in changes {
            safe_path(&path)?;
            files.push(json!({"path":path,"change":change}));
        }
        if revision(&bridge_git::take_snapshot(root).map_err(|_| "Git проекта недоступен")?)
            != revision(&current)
        {
            return Err("Файлы изменились — обновите diff");
        }
        let total = files.len();
        files.truncate(500);
        Ok(
            json!({"files":files,"total":total,"revision":revision(&current),"repositories":[],"mode":"project"}),
        )
    }
    pub fn project_diff_file(
        &self,
        project: &str,
        scope: &str,
        path: &str,
        expected_revision: &str,
    ) -> Result<Value> {
        self.project_diff_file_at(project, scope, path, expected_revision, None)
    }
    pub fn project_diff_file_at(
        &self,
        project: &str,
        scope: &str,
        path: &str,
        expected_revision: &str,
        checkout: Option<&crate::ConsoleCheckout>,
    ) -> Result<Value> {
        let _activity = self.activity_guard()?;
        check_scope(scope)?;
        safe_path(path)?;
        let (entry, _) = self.project(project)?;
        if checkout.is_none()
            && let Some(remote) = entry.remote_execution()
        {
            return bridge_automation::remote::rpc(remote,&json!({"op":"project_diff_file","scope":scope,"path":path,"revision":expected_revision})).map_err(|_|"Diff второго ПК недоступен; обновите agent-bridge и проверьте SSH");
        }
        let selected = checkout
            .map(|checkout| self.resolve_console_checkout(project, checkout))
            .transpose()?;
        let root = selected.as_deref().unwrap_or(entry.workspace());
        let current = bridge_git::take_snapshot(root).map_err(|_| "Git проекта недоступен")?;
        if revision(&current) != expected_revision {
            return Err("Файлы изменились — обновите diff");
        }
        let changes =
            bridge_git::diff::project_changes(root, scope, current.head().map(|h| h.as_str()))
                .map_err(|_| "Не удалось прочитать изменения Git")?;
        if !changes.iter().any(|(name, _)| name == path) {
            return Err("Файл не входит в выбранные изменения");
        }
        let before = if scope == "unstaged" {
            bridge_git::diff::index_file(root, path)
        } else {
            current
                .head()
                .map(|h| commit_file(root, h.as_str(), path))
                .transpose()
                .map(Option::flatten)
        }
        .map_err(|_| "Исходный файл недоступен; проверьте конфликты Git")?;
        let after = if scope == "staged" {
            bridge_git::diff::index_file(root, path)
                .map_err(|_| "Файл в индексе недоступен; проверьте конфликты Git")?
        } else {
            read_file(root, path)?
        };
        if revision(&bridge_git::take_snapshot(root).map_err(|_| "Git проекта недоступен")?)
            != expected_revision
        {
            return Err("Файлы изменились — обновите diff");
        }
        if before.as_ref().is_some_and(Option::is_none)
            || after.as_ref().is_some_and(Option::is_none)
        {
            return Ok(
                json!({"path":path,"kind":"large","message":"Файл больше 256 КиБ; построчный diff недоступен","patch":"","truncated":false}),
            );
        }
        let after = after.flatten();
        if scope != "staged" {
            let actual = after.as_ref().map(digest);
            let expected = current
                .manifest()
                .entries()
                .iter()
                .find(|e| e.path() == OsStr::new(path))
                .map(|e| e.digest_hex());
            if actual != expected {
                return Err("Файл изменился — обновите diff");
            }
        }
        let response = render_file(path, before.flatten(), after)?;
        if revision(&bridge_git::take_snapshot(root).map_err(|_| "Git проекта недоступен")?)
            != expected_revision
        {
            return Err("Файлы изменились — обновите diff");
        }
        Ok(response)
    }
    pub fn task_diff(&self, project: &str, task: &str, repository: u32) -> Result<Value> {
        let _activity = self.activity_guard()?;
        let (entry, _) = self.project(project)?;
        if let Some(remote) = entry.remote_execution() {
            return bridge_automation::remote::rpc(
                remote,
                &json!({"op":"diff","task":task,"repository":repository}),
            )
            .map_err(|_| "Diff на втором ПК недоступен; проверьте SSH и обновите agent-bridge");
        }
        let context = self.diff_context(project, task, repository)?;
        let current = bridge_git::take_snapshot(&context.root)
            .map_err(|_| "Не удалось прочитать изменения")?;
        let names = context
            .baseline
            .manifest()
            .entries()
            .iter()
            .chain(current.manifest().entries())
            .map(|e| e.path().to_str().ok_or("Имя файла не поддерживается"))
            .collect::<Result<BTreeSet<_>>>()?;
        let mut files = vec![];
        for path in names {
            let before = context.baseline.manifest().digest(OsStr::new(path));
            let after = current.manifest().digest(OsStr::new(path));
            if before == after {
                continue;
            }
            safe_path(path)?;
            files.push(json!({"path":path,"change":if before.is_none(){"added"}else if after.is_none(){"deleted"}else{"modified"}}));
        }
        let total = files.len();
        files.truncate(500);
        Ok(
            json!({"files":files,"total":total,"revision":revision(&current),"repositories":context.repositories,"mode":context.mode}),
        )
    }
    pub fn task_diff_file(
        &self,
        project: &str,
        task: &str,
        repository: u32,
        path: &str,
        expected_revision: &str,
    ) -> Result<Value> {
        let _activity = self.activity_guard()?;
        safe_path(path)?;
        let (entry, _) = self.project(project)?;
        if let Some(remote) = entry.remote_execution() {
            return bridge_automation::remote::rpc(remote,&json!({"op":"diff_file","task":task,"repository":repository,"path":path,"revision":expected_revision})).map_err(|_|"Diff на втором ПК недоступен; проверьте SSH и обновите agent-bridge");
        }
        let context = self.diff_context(project, task, repository)?;
        let current = bridge_git::take_snapshot(&context.root)
            .map_err(|_| "Не удалось прочитать изменения")?;
        if revision(&current) != expected_revision {
            return Err("Файлы изменились — обновите список изменений");
        }
        let base_digest = context.baseline.manifest().digest(OsStr::new(path));
        if base_digest == current.manifest().digest(OsStr::new(path)) {
            return Err("Файл не входит в изменения задачи");
        }
        let message = |kind: &str, text: &str| {
            Ok(json!({"path":path,"kind":kind,"message":text,"patch":"","truncated":false}))
        };
        let before = if base_digest.is_none() {
            None
        } else {
            let candidate = context
                .baseline
                .head()
                .map(|head| commit_file(&context.root, head.as_str(), path))
                .transpose()
                .map_err(|_| "Исходный файл недоступен")?
                .flatten();
            if candidate.as_ref().is_some_and(Option::is_none) {
                return message(
                    "large",
                    "Исходный файл больше 256 КиБ; построчный diff недоступен",
                );
            }
            let mut candidate = candidate.flatten();
            let expected = context
                .baseline
                .manifest()
                .entries()
                .iter()
                .find(|e| e.path() == OsStr::new(path))
                .unwrap()
                .digest_hex();
            if candidate
                .as_ref()
                .is_none_or(|object| digest(object) != expected)
            {
                candidate = match &context.parent {
                    Some(parent) => read_file(parent, path)?.flatten(),
                    None => None,
                };
                if candidate
                    .as_ref()
                    .is_none_or(|object| digest(object) != expected)
                {
                    return message(
                        "baseline_unavailable",
                        "Исходные строки незакоммиченного файла не сохранены; точный diff недоступен",
                    );
                }
            }
            candidate
        };
        let after = read_file(&context.root, path)?;
        if after.as_ref().is_some_and(Option::is_none) {
            return message("large", "Файл больше 256 КиБ; построчный diff недоступен");
        }
        let after = after.flatten();
        let actual = after.as_ref().map(digest);
        let expected = current
            .manifest()
            .entries()
            .iter()
            .find(|entry| entry.path() == OsStr::new(path))
            .map(|entry| entry.digest_hex());
        if actual != expected {
            return Err("Файл изменился — обновите список изменений");
        }
        let response = render_file(path, before, after)?;
        if revision(
            &bridge_git::take_snapshot(&context.root)
                .map_err(|_| "Не удалось проверить изменения")?,
        ) != expected_revision
        {
            return Err("Файлы изменились — обновите список изменений");
        }
        Ok(response)
    }
}

fn render_file(
    path: &str,
    before: Option<bridge_git::objects::CommitObject>,
    after: Option<bridge_git::objects::CommitObject>,
) -> Result<Value> {
    let message = |kind: &str, text: &str| {
        Ok(json!({"path":path,"kind":kind,"message":text,"patch":"","truncated":false}))
    };
    let old = before.as_ref().map_or(&[][..], |o| o.data.as_slice());
    let new = after.as_ref().map_or(&[][..], |o| o.data.as_slice());
    if old.contains(&0)
        || new.contains(&0)
        || std::str::from_utf8(old).is_err()
        || std::str::from_utf8(new).is_err()
    {
        return message(
            "binary",
            "Бинарный файл или текст с неподдерживаемой кодировкой",
        );
    }
    if [old, new].iter().any(|data| {
        !bridge_config::suspected_secret_categories(std::str::from_utf8(data).unwrap()).is_empty()
    }) {
        return message("redacted", "Содержимое скрыто: возможные секреты");
    }
    let (patch, truncated) = text_patch(old, new).map_err(|_| "Не удалось построить diff")?;
    Ok(
        json!({"path":path,"kind":"text","patch":patch,"truncated":truncated,"before_mode":before.map(|o|format!("{:o}",o.mode)),"after_mode":after.map(|o|format!("{:o}",o.mode))}),
    )
}

fn check_scope(scope: &str) -> Result<()> {
    if matches!(scope, "all" | "staged" | "unstaged") {
        Ok(())
    } else {
        Err("Недопустимый режим diff")
    }
}
