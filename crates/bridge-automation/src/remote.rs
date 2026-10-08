//! SSH transport and immutable Git review snapshots. No credentials travel in RPC.
use crate::codex::{CodexError, Operation, private_dir, read_bounded, validate_answer};
use crate::coordinator::ReviewClient;
use bridge_config::remote::RemoteExecution;
use bridge_storage::{
    RustStateLayout,
    automation::{AutomationRunStore, RunId},
};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};
const LIMIT: usize = 8 * 1024 * 1024;
pub fn ssh(settings: &RemoteExecution, command: &str) -> Result<Command, String> {
    settings.validate().map_err(|e| e.to_string())?;
    if !matches!(
        command,
        "remote-rpc" | "mcp" | "console" | "attach-opencode" | "launch-opencode"
    ) {
        return Err("remote_command_invalid".into());
    }
    let quote = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
    let argv = [
        &settings.executable,
        command,
        "--project",
        &settings.project,
        "--config",
        &settings.config,
        "--state-root",
        &settings.state_root,
    ];
    let mut cmd = Command::new("ssh");
    cmd.args([
        "-T",
        "-o",
        "BatchMode=yes",
        "-o",
        "StrictHostKeyChecking=yes",
        "-o",
        "ConnectTimeout=5",
        "-o",
        "ServerAliveInterval=5",
        "-o",
        "ServerAliveCountMax=2",
        "-p",
        &settings.port.to_string(),
        "-l",
        &settings.user,
        "--",
        &settings.host,
    ]);
    cmd.arg(argv.iter().map(|v| quote(v)).collect::<Vec<_>>().join(" "));
    Ok(cmd)
}
/// Bounded subprocess transport with an explicit deadline; stderr never enters UI.
pub fn execute(mut cmd: Command, input: Vec<u8>, timeout: Duration) -> Result<Vec<u8>, String> {
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().map_err(|_| "remote_process_unavailable")?;
    let mut stdin = child.stdin.take().ok_or("remote_stdin_unavailable")?;
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let stdout = child.stdout.take().ok_or("remote_stdout_unavailable")?;
    let (tx, rx) = mpsc::sync_channel(1);
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout.take(LIMIT as u64 + 1).read_to_end(&mut bytes);
        let _ = tx.send(if result.is_ok() && bytes.len() <= LIMIT {
            Ok(bytes)
        } else {
            Err("remote_output_limit")
        });
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                break Err("remote_connection_timeout");
            }
        }
    };
    let _ = child.wait();
    // ssh/git own their output pipes; do not join an untrusted surviving descendant.
    let bytes = rx
        .recv_timeout(Duration::from_secs(1))
        .map_err(|_| "remote_connection_lost")??;
    drop(reader);
    drop(writer);
    if !status?.success() {
        return Err(
            "remote_command_failed: check SSH access, agent-bridge and Git authentication".into(),
        );
    }
    Ok(bytes)
}
pub fn rpc(settings: &RemoteExecution, value: &Value) -> Result<Value, String> {
    let mut input = serde_json::to_vec(value).map_err(|_| "remote_request_invalid")?;
    if input.len() > 1_000_000 {
        return Err("remote_request_limit".into());
    }
    input.push(b'\n');
    let bytes = execute(ssh(settings, "remote-rpc")?, input, Duration::from_secs(35))?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| "remote_response_invalid")?;
    if let Some(error) = value["error"].as_str() {
        return Err(error.to_owned());
    }
    Ok(value)
}
fn git(root: &Path, args: &[&str], input: &[u8], index: Option<&Path>) -> Result<String, String> {
    let mut cmd = Command::new("git");
    cmd.current_dir(root)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0");
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    ] {
        cmd.env_remove(key);
    }
    if let Some(index) = index {
        cmd.env("GIT_INDEX_FILE", index);
    }
    cmd.env("GIT_AUTHOR_NAME", "AIbridge")
        .env("GIT_AUTHOR_EMAIL", "aibridge@localhost")
        .env("GIT_COMMITTER_NAME", "AIbridge")
        .env("GIT_COMMITTER_EMAIL", "aibridge@localhost")
        .env("GIT_AUTHOR_DATE", "@0 +0000")
        .env("GIT_COMMITTER_DATE", "@0 +0000");
    String::from_utf8(execute(cmd, input.to_vec(), Duration::from_secs(30))?)
        .map(|s| s.trim().to_owned())
        .map_err(|_| "remote_git_output_invalid".into())
}
fn oid(s: &str) -> bool {
    matches!(s.len(), 40 | 64)
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub fn publish(
    root: &Path,
    directory: &Path,
    repository: &str,
    run: RunId,
) -> Result<Value, String> {
    private_dir(directory).map_err(|_| "remote_snapshot_directory_invalid")?;
    let before = bridge_git::take_snapshot(root).map_err(|_| "remote_repository_invalid")?;
    let base = before
        .head()
        .ok_or("remote_head_missing")?
        .as_str()
        .to_owned();
    bridge_git::checkout::check_supported(root, &base)
        .map_err(|_| "remote_repository_unsupported")?;
    let index = directory.join(format!("index-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        git(root, &["read-tree", &base], b"", Some(&index))?;
        git(root, &["add", "--all", "--", "."], b"", Some(&index))?;
        let tree = git(root, &["write-tree"], b"", Some(&index))?;
        if !oid(&tree) {
            return Err("remote_tree_invalid".into());
        }
        let commit = git(
            root,
            &["commit-tree", &tree, "-p", &base],
            b"AIbridge review snapshot\n",
            Some(&index),
        )?;
        if !oid(&commit) {
            return Err("remote_commit_invalid".into());
        }
        let reference = format!("refs/heads/aibridge/{run}/{tree}");
        git(
            root,
            &["push", "--", repository, &format!("{commit}:{reference}")],
            b"",
            None,
        )?;
        let after = bridge_git::take_snapshot(root).map_err(|_| "remote_repository_invalid")?;
        if before != after {
            return Err("remote_snapshot_changed".into());
        }
        Ok(json!({"commit":commit,"base":base,"reference":reference}))
    })();
    let _ = std::fs::remove_file(&index);
    let _ = std::fs::remove_file(index.with_extension("lock"));
    result
}
/// Materialize a snapshot as a dirty checkout of its pinned parent for ordinary
/// independent Codex diff review. Caller receives a fresh checkout per request.
pub fn fetch_review(
    directory: &Path,
    repository: &str,
    snapshot: &Value,
) -> Result<PathBuf, String> {
    let commit = snapshot["commit"]
        .as_str()
        .filter(|s| oid(s))
        .ok_or("remote_snapshot_invalid")?;
    let base = snapshot["base"]
        .as_str()
        .filter(|s| oid(s))
        .ok_or("remote_snapshot_invalid")?;
    let reference = snapshot["reference"]
        .as_str()
        .filter(|s| {
            s.starts_with("refs/heads/aibridge/")
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"/_-".contains(&b))
        })
        .ok_or("remote_ref_invalid")?;
    private_dir(directory).map_err(|_| "remote_review_directory_invalid")?;
    let root = directory.join(uuid::Uuid::new_v4().to_string());
    private_dir(&root).map_err(|_| "remote_review_directory_invalid")?;
    git(&root, &["init", "-q"], b"", None)?;
    git(
        &root,
        &["fetch", "--no-tags", "--", repository, reference],
        b"",
        None,
    )?;
    if git(&root, &["rev-parse", "FETCH_HEAD"], b"", None)? != commit
        || git(&root, &["rev-parse", &format!("{commit}^")], b"", None)? != base
    {
        return Err("remote_snapshot_sha_mismatch".into());
    }
    bridge_git::checkout::check_supported(&root, commit)
        .map_err(|_| "remote_repository_unsupported")?;
    git(&root, &["checkout", "--detach", base], b"", None)?;
    git(&root, &["read-tree", "--reset", "-u", commit], b"", None)?;
    git(&root, &["read-tree", base], b"", None)?;
    Ok(root)
}
/// B-side model adapter. Requests/answers survive SSH reconnects. Worker never
/// executes Codex; it waits for the first PC's independently validated answer.
pub struct RemoteReview {
    pub layout: RustStateLayout,
    pub run: RunId,
    pub repository: String,
}
impl ReviewClient for RemoteReview {
    fn call(
        &mut self,
        kind: Operation,
        workspace: &Path,
        context: &Value,
        timeout: Duration,
        cancelled: &mut dyn FnMut() -> bool,
    ) -> Result<Value, CodexError> {
        let dir = crate::lifecycle::directory(&self.layout, self.run).join("remote");
        private_dir(&dir)?;
        let fingerprint = bridge_artifact::fingerprint(
            &bridge_git::take_snapshot(workspace).map_err(|_| CodexError::Io)?,
        );
        let snapshot =
            publish(workspace, &dir, &self.repository, self.run).map_err(|_| CodexError::Io)?;
        let nonce = uuid::Uuid::new_v4().to_string();
        let request = json!({"nonce":nonce,"operation":kind.as_str(),"context":context,"snapshot":snapshot,"timeout":timeout.as_secs(),"run_id":self.run.to_string()});
        bridge_artifact::artifact::atomic_write(
            &dir.join("request.json"),
            request.to_string().as_bytes(),
            0o600,
        )
        .map_err(|_| CodexError::Io)?;
        let answer = dir.join(format!("answer-{nonce}.json"));
        let deadline = Instant::now() + timeout;
        let result = loop {
            if cancelled() {
                break Err(CodexError::Cancelled);
            }
            if Instant::now() >= deadline {
                break Err(CodexError::Timeout);
            }
            if answer.exists() {
                let bytes = read_bounded(&answer, 1_000_000)?;
                let value = serde_json::from_slice(&bytes).map_err(|_| CodexError::Result)?;
                break validate_answer(kind, value);
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        let _ = std::fs::remove_file(dir.join("request.json"));
        if kind == Operation::Review && result.as_ref().is_ok_and(|a| a["decision"] == "accept") {
            let mut doc = AutomationRunStore::new(self.layout.clone())
                .load(Some(self.run))
                .map_err(|_| CodexError::Io)?
                .document()
                .clone();
            doc["remote_result"] = snapshot;
            doc["remote_result"]["task_id"] = context["task_id"].clone();
            doc["remote_result"]["round"] = context["round"].clone();
            doc["remote_result"]["fingerprint"] = fingerprint;
            // Do not save coordinator's document out of band; accepted snapshot
            // is a separate immutable receipt used when the run becomes ready.
            bridge_artifact::artifact::atomic_write(
                &dir.join("accepted.json"),
                doc["remote_result"].to_string().as_bytes(),
                0o600,
            )
            .map_err(|_| CodexError::Io)?;
        }
        result
    }
}

/// Durable, resumable file delivery on PC A. Git HEAD and index are never changed.
/// An entry may be in its original or already delivered state; any third state
/// blocks resume rather than overwriting concurrent user changes.
pub fn apply_result(
    project: &bridge_config::ProjectEntry,
    layout: &RustStateLayout,
    id: RunId,
    settings: &RemoteExecution,
) -> Result<(), String> {
    use bridge_artifact::artifact as a;
    let store = AutomationRunStore::new(layout.clone());
    let run = store.load(Some(id)).map_err(|e| e.to_string())?;
    crate::run::check_config_binding(project, layout, &run).map_err(|e| e.to_string())?;
    let directory = crate::lifecycle::directory(layout, id).join("remote-delivery");
    let path = directory.join("artifact");
    private_dir(&directory).map_err(|_| "remote_delivery_directory_invalid")?;
    if !path.exists() {
        crate::run::check_binding(project, layout, &run).map_err(|e| e.to_string())?;
        let checkout = fetch_review(
            &directory.join("reviews"),
            &settings.repository,
            &run.document()["remote_result"],
        )?;
        let baseline =
            bridge_git::RepositorySnapshot::from_json(&run.document()["origin"]["snapshot"])
                .map_err(|_| "remote_origin_invalid")?;
        let scopes = run.document()["steps"]
            .as_array()
            .and_then(|s| s.last())
            .and_then(|s| s["step"]["allowed_paths"].as_array())
            .ok_or("remote_scope_invalid")?
            .iter()
            .map(|s| s.as_str().map(str::to_owned).ok_or("remote_scope_invalid"))
            .collect::<Result<Vec<_>, _>>()?;
        let (artifact, blobs) = a::build_entries(
            &checkout,
            id.to_string()
                .parse()
                .map_err(|_| "remote_identity_invalid")?,
            run.document()["remote_result"]["base"]
                .as_str()
                .ok_or("remote_base_missing")?,
            &baseline,
            &scopes,
        )
        .map_err(|e| e.to_string())?;
        // Publish the entire immutable journal before the first target file write.
        let pending = directory.join(format!("pending-{}", uuid::Uuid::new_v4()));
        a::write_artifact(&pending, &artifact, &blobs).map_err(|e| e.to_string())?;
        std::fs::rename(pending, &path).map_err(|_| "remote_journal_unwritable")?;
        a::sync_dir(&directory).map_err(|e| e.to_string())?;
    }
    let artifact = a::load_artifact(&path).map_err(|e| e.to_string())?;
    if artifact.task_id.to_string() != id.to_string()
        || artifact.base_head
            != run.document()["remote_result"]["base"]
                .as_str()
                .ok_or("remote_base_missing")?
    {
        return Err("remote_delivery_binding_changed".into());
    }
    let snapshot =
        bridge_git::take_snapshot(project.workspace()).map_err(|_| "remote_repository_invalid")?;
    let expected = &run.document()["origin"]["snapshot"];
    let actual = snapshot
        .to_json()
        .map_err(|_| "remote_repository_invalid")?;
    if actual["head"] != expected["head"]
        || actual["index_fingerprint"] != expected["index_fingerprint"]
    {
        return Err("remote_main_head_or_index_changed".into());
    }
    let baseline =
        bridge_git::RepositorySnapshot::from_json(expected).map_err(|_| "remote_origin_invalid")?;
    let paths = artifact
        .entries
        .iter()
        .map(|e| e.path.clone())
        .collect::<Vec<_>>();
    let comparison = bridge_git::compare_repository_snapshot(
        project.workspace(),
        &baseline,
        &paths,
        true,
        false,
    )
    .map_err(|_| "remote_repository_invalid")?;
    if !comparison.scope_violations().is_empty() {
        return Err("remote_main_has_unrelated_changes".into());
    }
    for entry in &artifact.entries {
        let current =
            a::read_object(project.workspace(), &entry.path).map_err(|e| e.to_string())?;
        if !entry.matches_base(current.as_ref()) && !entry.matches_artifact(current.as_ref()) {
            return Err("remote_delivery_conflict".into());
        }
    }
    let mut entries = artifact.entries.clone();
    entries.sort_by(|a, b| {
        (a.op != "delete").cmp(&(b.op != "delete")).then_with(|| {
            if a.op == "delete" {
                b.path.len().cmp(&a.path.len())
            } else {
                a.path.len().cmp(&b.path.len())
            }
        })
    });
    for entry in &entries {
        let control = store.load(Some(id)).map_err(|e| e.to_string())?.control();
        if control != bridge_storage::automation::RunControl::Run {
            return Err("automation_cancelled".into());
        }
        a::parents(project.workspace(), &entry.path).map_err(|e| e.to_string())?;
        let current =
            a::read_object(project.workspace(), &entry.path).map_err(|e| e.to_string())?;
        if entry.matches_artifact(current.as_ref()) {
            continue;
        }
        if !entry.matches_base(current.as_ref()) {
            return Err("remote_delivery_conflict".into());
        }
        let target = project.workspace().join(&entry.path);
        if entry.op == "delete" {
            std::fs::remove_file(&target).map_err(|_| "remote_delivery_unwritable")?;
        } else {
            let blob = entry.blob_sha256.as_deref().ok_or("remote_blob_missing")?;
            let data = a::read_file(&path.join("blobs").join(blob)).map_err(|e| e.to_string())?;
            if a::digest(&data) != blob {
                return Err("remote_blob_corrupt".into());
            }
            // parents() just proved no parent component traverses a symlink.
            std::fs::create_dir_all(target.parent().ok_or("remote_path_invalid")?)
                .map_err(|_| "remote_delivery_unwritable")?;
            if entry.kind == "symlink" {
                use std::os::unix::ffi::OsStrExt;
                let temporary =
                    target.with_file_name(format!(".aibridge-{}", uuid::Uuid::new_v4()));
                std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(&data), &temporary)
                    .map_err(|_| "remote_delivery_unwritable")?;
                std::fs::rename(temporary, &target).map_err(|_| "remote_delivery_unwritable")?;
            } else {
                a::atomic_write(&target, &data, entry.mode & 0o777).map_err(|e| e.to_string())?;
            }
        }
        a::sync_dir(target.parent().ok_or("remote_path_invalid")?).map_err(|e| e.to_string())?;
    }
    for entry in &entries {
        if !entry.matches_artifact(
            a::read_object(project.workspace(), &entry.path)
                .map_err(|e| e.to_string())?
                .as_ref(),
        ) {
            return Err("remote_delivery_incomplete".into());
        }
    }
    Ok(())
}

/// Called only under remote admission and automation fences, before the run's
/// origin is recorded. Dirty files or an unfinished run are never discarded.
pub fn synchronize_source(
    project: &bridge_config::ProjectEntry,
    metadata: &Value,
) -> Result<(), String> {
    let root = project.workspace();
    crate::run::origin(project).map_err(|_| "remote_source_requires_clean_main_repository")?;
    let before = bridge_git::take_snapshot(root).map_err(|_| "remote_repository_invalid")?;
    if !before.dirty_paths().is_empty() {
        return Err("remote_source_dirty".into());
    }
    let base = metadata["base"]
        .as_str()
        .filter(|s| oid(s))
        .ok_or("remote_source_invalid")?;
    let source = &metadata["source"];
    if source.is_null() {
        if before.head().map(|h| h.as_str()) != Some(base) {
            return Err("remote_base_mismatch".into());
        }
        return Ok(());
    }
    let commit = source["commit"]
        .as_str()
        .filter(|s| oid(s))
        .ok_or("remote_source_invalid")?;
    let reference = source["reference"]
        .as_str()
        .filter(|s| {
            s.starts_with("refs/heads/aibridge/")
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"/_-".contains(&b))
        })
        .ok_or("remote_ref_invalid")?;
    git(
        root,
        &[
            "fetch",
            "--no-tags",
            "--",
            metadata["repository"]
                .as_str()
                .ok_or("remote_repository_missing")?,
            reference,
        ],
        b"",
        None,
    )?;
    if git(root, &["rev-parse", "FETCH_HEAD"], b"", None)? != commit
        || git(root, &["rev-parse", &format!("{commit}^")], b"", None)? != base
        || source["base"] != base
    {
        return Err("remote_source_sha_mismatch".into());
    }
    bridge_git::checkout::check_supported(root, base)
        .map_err(|_| "remote_repository_unsupported")?;
    if bridge_git::take_snapshot(root).map_err(|_| "remote_repository_invalid")? != before {
        return Err("remote_source_changed".into());
    }
    if before.head().map(|h| h.as_str()) != Some(base) {
        git(root, &["checkout", "--detach", base], b"", None)?;
    }
    Ok(())
}

/// Terminal sessions are run on the executor with an SSH PTY. The only appended
/// argument is a typed task UUID, never a caller-provided shell fragment.
pub fn attach(
    settings: &RemoteExecution,
    command: &str,
    task: Option<bridge_domain::TaskId>,
) -> Result<std::process::ExitStatus, String> {
    let base = ssh(settings, command)?;
    let mut argv = base
        .get_args()
        .map(|s| s.to_os_string())
        .collect::<Vec<_>>();
    if let Some(first) = argv.first_mut() {
        *first = "-tt".into();
    }
    if let Some(task) = task {
        let last = argv.last_mut().ok_or("remote_command_invalid")?;
        last.push(format!(" --task '{task}'"));
    }
    Command::new("ssh")
        .args(argv)
        .status()
        .map_err(|_| "remote_terminal_unavailable".into())
}
