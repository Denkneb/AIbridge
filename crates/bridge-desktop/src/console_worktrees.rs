//! Validated standalone console checkouts; task worktrees remain worker-owned.
use crate::projects::ProjectService;
use bridge_git::{branches::repository_root, console_worktrees};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConsoleCheckout {
    pub workspace: String,
    pub current: Option<String>,
}

#[derive(Serialize)]
pub struct ConsoleCheckoutView {
    pub workspace: String,
    pub current: Option<String>,
    pub kind: &'static str,
    pub available: bool,
}

impl ProjectService {
    fn console_kind(&self, path: &Path, main: &Path, owned: &Path, named: bool) -> &'static str {
        if path == main {
            "main"
        } else if path.starts_with(owned) && named {
            "codex"
        } else if path.starts_with(&self.state) || !named {
            "task"
        } else {
            "existing"
        }
    }

    pub fn console_checkouts(
        &self,
        project: &str,
    ) -> Result<Vec<ConsoleCheckoutView>, &'static str> {
        let _activity = self.activity_guard()?;
        let (entry, layout) = self.project(project)?;
        let main = repository_root(entry.workspace()).map_err(|_| "Git проекта недоступен")?;
        let owned = layout.project_dir().join("codex-worktrees");
        console_worktrees::list(&main)
            .map_err(|_| "Не удалось прочитать worktree")?
            .into_iter()
            .map(|record| {
                let kind = self.console_kind(&record.path, &main, &owned, record.branch.is_some());
                let available =
                    kind != "task" && !record.locked && !record.prunable && record.path.is_dir();
                Ok(ConsoleCheckoutView {
                    workspace: record
                        .path
                        .to_str()
                        .ok_or("Путь worktree имеет неподдерживаемую кодировку")?
                        .into(),
                    current: record.branch,
                    kind,
                    available,
                })
            })
            .collect()
    }

    /// Accept only a registered, accessible checkout of this project's repository.
    pub fn resolve_console_checkout(
        &self,
        project: &str,
        selection: &ConsoleCheckout,
    ) -> Result<PathBuf, &'static str> {
        let (entry, _) = self.project(project)?;
        let main = repository_root(entry.workspace()).map_err(|_| "Git проекта недоступен")?;
        let path = PathBuf::from(&selection.workspace);
        if !path.is_absolute()
            || fs::canonicalize(&path).map_err(|_| "Worktree отсутствует")? != path
        {
            return Err("Worktree перемещён или заменён ссылкой");
        }
        let record = self
            .console_checkouts(project)?
            .into_iter()
            .find(|record| record.workspace == selection.workspace && record.available)
            .ok_or("Worktree не принадлежит проекту или является служебным каталогом задачи")?;
        if record.current != selection.current {
            return Err("Ветка worktree изменилась. Выберите рабочую папку заново");
        }
        if repository_root(&path).map_err(|_| "Worktree недоступен")? != path
            || bridge_git::checkout::main_common_dir(&path).map_err(|_| "Worktree недоступен")?
                != bridge_git::checkout::main_common_dir(&main)
                    .map_err(|_| "Git проекта недоступен")?
        {
            return Err("Worktree не принадлежит Git-репозиторию проекта");
        }
        Ok(path)
    }

    pub fn create_console_checkout(
        &self,
        project: &str,
        branch: &str,
        base: Option<&str>,
        expected_workspace: &str,
    ) -> Result<ConsoleCheckout, &'static str> {
        let _activity = self.activity_guard()?;
        let _config = self.config_file_guard()?;
        let (entry, layout) = self.project(project)?;
        if entry.workspace().to_str() != Some(expected_workspace) {
            return Err("Workspace проекта изменился. Обновите список");
        }
        let main = repository_root(entry.workspace()).map_err(|_| "Git проекта недоступен")?;
        let parent = layout.project_dir().join("codex-worktrees");
        fs::create_dir_all(&parent).map_err(|_| "Не удалось создать каталог worktree")?;
        if fs::canonicalize(&parent).map_err(|_| "Каталог worktree недоступен")? != parent
            || parent.starts_with(&main)
        {
            return Err(
                "Каталог worktree должен находиться вне основного checkout и не быть ссылкой",
            );
        }
        let path = parent.join(uuid::Uuid::new_v4().to_string());
        console_worktrees::create(&main, &path, branch, base)
            .map_err(|_| "Не удалось создать worktree: проверьте имя ветки, исходную ветку и занятость ветки другим worktree")?;
        let selection = ConsoleCheckout {
            workspace: path
                .to_str()
                .ok_or("Путь worktree имеет неподдерживаемую кодировку")?
                .into(),
            current: Some(format!("refs/heads/{branch}")),
        };
        self.resolve_console_checkout(project, &selection)?;
        Ok(selection)
    }
}
