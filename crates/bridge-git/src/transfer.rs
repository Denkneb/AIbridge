//! Explicit single-branch Git transfers. No shell, force push or implicit tags.
use crate::{GitError, branches, run_checked, run_git};
use process_wrap::std::{CommandWrap, ProcessSession};
use std::{
    io::Read,
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

pub fn remotes(workspace: &Path) -> Result<Vec<String>, GitError> {
    let bytes = run_checked(workspace, &["remote"])?;
    Ok(std::str::from_utf8(&bytes)
        .map_err(|_| GitError::MalformedOutput)?
        .lines()
        .map(str::to_owned)
        .collect())
}
pub fn valid_name(workspace: &Path, name: &str) -> Result<(), GitError> {
    if name.is_empty() || name.starts_with('-') || name.contains("@{") || name == "HEAD" {
        return Err(GitError::CommandFailed);
    }
    run_checked(
        workspace,
        &["check-ref-format", &format!("refs/heads/{name}")],
    )?;
    Ok(())
}
pub fn create(workspace: &Path, name: &str, base: &str, switch: bool) -> Result<(), GitError> {
    valid_name(workspace, name)?;
    let state = branches::branches(workspace)?;
    if state
        .branches
        .iter()
        .any(|b| b.reference == format!("refs/heads/{name}"))
    {
        return Err(GitError::CommandFailed);
    }
    let start = if base == "HEAD" {
        state.head.ok_or(GitError::CommandFailed)?
    } else {
        if !state.branches.iter().any(|b| b.reference == base) {
            return Err(GitError::CommandFailed);
        }
        String::from_utf8(run_checked(
            workspace,
            &["rev-parse", "--verify", &format!("{base}^{{commit}}")],
        )?)
        .map_err(|_| GitError::MalformedOutput)?
        .trim()
        .into()
    };
    if switch {
        if !crate::status_porcelain(workspace)?.is_empty()
            || branches::operation_in_progress(workspace)?
        {
            return Err(GitError::CommandFailed);
        }
        run_checked(
            workspace,
            &[
                "-c",
                "core.hooksPath=/dev/null",
                "switch",
                "--no-overwrite-ignore",
                "--no-track",
                "-c",
                name,
                &start,
            ],
        )?;
    } else {
        run_checked(
            workspace,
            &[
                "-c",
                "core.hooksPath=/dev/null",
                "branch",
                "--no-track",
                "--",
                name,
                &start,
            ],
        )?;
    }
    Ok(())
}
fn require_remote(workspace: &Path, remote: &str) -> Result<(), GitError> {
    if !remotes(workspace)?.iter().any(|name| name == remote) {
        return Err(GitError::CommandFailed);
    }
    Ok(())
}
/// Bounded network subprocess including SSH/helper descendants and stdout.
fn network(workspace: &Path, args: &[&str]) -> Result<Vec<u8>, GitError> {
    network_with_timeout(workspace, args, Duration::from_secs(30))
}
fn network_with_timeout(
    workspace: &Path,
    args: &[&str],
    timeout: Duration,
) -> Result<Vec<u8>, GitError> {
    let mut command = Command::new("git");
    command
        .args(["-c", "core.hooksPath=/dev/null", "-c", "gc.auto=0"])
        .args(args)
        .current_dir(workspace)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_SSH_COMMAND", "ssh -oBatchMode=yes -oConnectTimeout=10")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = CommandWrap::from(command)
        .wrap(ProcessSession)
        .spawn()
        .map_err(|_| GitError::Spawn)?;
    let pipe = child.stdout().take().ok_or(GitError::Io)?;
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = pipe.take(1024 * 1024 + 1).read_to_end(&mut bytes);
        let _ = tx.send((result, bytes));
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) => break Err(GitError::Timeout),
            Err(_) => break Err(GitError::Wait),
        }
    };
    let _ = child.kill();
    let _ = child.wait();
    reader.join().map_err(|_| GitError::Io)?;
    let status = status?;
    let (result, bytes) = rx.recv().map_err(|_| GitError::Io)?;
    result.map_err(|_| GitError::Io)?;
    if !status.success() {
        return Err(GitError::CommandFailed);
    }
    if bytes.len() > 1024 * 1024 {
        return Err(GitError::MalformedOutput);
    }
    Ok(bytes)
}
pub fn fetch(workspace: &Path, remote: &str) -> Result<(), GitError> {
    require_remote(workspace, remote)?;
    // Explicit heads-only refspec avoids configured mappings into local branches.
    network(
        workspace,
        &[
            "fetch",
            "--no-tags",
            "--no-recurse-submodules",
            "--",
            remote,
            &format!("+refs/heads/*:refs/remotes/{remote}/*"),
        ],
    )?;
    Ok(())
}
#[derive(PartialEq, Eq)]
pub struct PushPlan {
    pub current: String,
    pub head: String,
    pub remote: String,
    pub destination: String,
    pub remote_head: Option<String>,
    pub ahead: usize,
    pub behind: usize,
    pub new_branch: bool,
    pub set_upstream: bool,
    pub fingerprint: String,
    url: String,
}
pub fn upstream(workspace: &Path) -> Result<Option<(String, String)>, GitError> {
    let Some(current) = branches::branches(workspace)?.current else {
        return Ok(None);
    };
    let name = current
        .strip_prefix("refs/heads/")
        .ok_or(GitError::MalformedOutput)?;
    let remote = run_git(
        workspace,
        &["config", "--get", &format!("branch.{name}.remote")],
    )?;
    let merge = run_git(
        workspace,
        &["config", "--get", &format!("branch.{name}.merge")],
    )?;
    if !remote.status.success() || !merge.status.success() {
        return Ok(None);
    }
    let remote = std::str::from_utf8(&remote.stdout)
        .map_err(|_| GitError::MalformedOutput)?
        .trim();
    let reference = std::str::from_utf8(&merge.stdout)
        .map_err(|_| GitError::MalformedOutput)?
        .trim();
    Ok(reference
        .strip_prefix("refs/heads/")
        .filter(|_| !remote.is_empty())
        .map(|name| (remote.into(), name.into())))
}
pub fn preview(workspace: &Path, remote: &str, destination: &str) -> Result<PushPlan, GitError> {
    require_remote(workspace, remote)?;
    valid_name(workspace, destination)?;
    let state = branches::branches(workspace)?;
    let current = state.current.ok_or(GitError::CommandFailed)?;
    let head = state.head.ok_or(GitError::CommandFailed)?;
    let urls = run_checked(workspace, &["remote", "get-url", "--push", "--all", remote])?;
    let urls = std::str::from_utf8(&urls).map_err(|_| GitError::MalformedOutput)?;
    let urls: Vec<_> = urls.lines().collect();
    if urls.len() != 1 {
        return Err(GitError::CommandFailed);
    }
    let destination_ref = format!("refs/heads/{destination}");
    // Query the push URL, which can differ from the fetch URL. Never return it.
    let bytes = network(
        workspace,
        &["ls-remote", "--heads", "--", urls[0], &destination_ref],
    )?;
    let text = std::str::from_utf8(&bytes).map_err(|_| GitError::MalformedOutput)?;
    let remote_head = if text.trim().is_empty() {
        None
    } else {
        let lines: Vec<_> = text.lines().collect();
        if lines.len() != 1 {
            return Err(GitError::MalformedOutput);
        }
        let (id, reference) = lines[0].split_once('\t').ok_or(GitError::MalformedOutput)?;
        if reference != destination_ref
            || !matches!(id.len(), 40 | 64)
            || !id.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(GitError::MalformedOutput);
        }
        Some(id.to_owned())
    };
    let counts = if let Some(id) = &remote_head {
        run_checked(
            workspace,
            &[
                "rev-list",
                "--left-right",
                "--count",
                &format!("{head}...{id}"),
            ],
        )?
    } else {
        run_checked(workspace, &["rev-list", "--count", &head])?
    };
    let counts = std::str::from_utf8(&counts)
        .map_err(|_| GitError::MalformedOutput)?
        .split_whitespace()
        .map(str::parse::<usize>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| GitError::MalformedOutput)?;
    let ahead = *counts.first().ok_or(GitError::MalformedOutput)?;
    let behind = if remote_head.is_some() {
        *counts.get(1).ok_or(GitError::MalformedOutput)?
    } else {
        0
    };
    let set_upstream = upstream(workspace)?.is_none();
    let material = serde_json::json!([
        current,
        head,
        remote,
        destination,
        remote_head,
        urls,
        set_upstream
    ]);
    let fingerprint = crate::sha256::digest(material.to_string().as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok(PushPlan {
        current,
        head,
        remote: remote.into(),
        destination: destination.into(),
        new_branch: remote_head.is_none(),
        remote_head,
        ahead,
        behind,
        set_upstream,
        fingerprint,
        url: urls[0].into(),
    })
}
pub fn push(workspace: &Path, plan: &PushPlan) -> Result<bool, GitError> {
    let fresh = preview(workspace, &plan.remote, &plan.destination)?;
    if &fresh != plan || plan.behind > 0 {
        return Err(GitError::CommandFailed);
    }
    network(
        workspace,
        &[
            "push",
            "--porcelain",
            "--no-force",
            "--no-mirror",
            "--no-follow-tags",
            "--recurse-submodules=no",
            "--",
            &plan.url,
            &format!("{}:refs/heads/{}", plan.head, plan.destination),
        ],
    )?;
    if plan.set_upstream {
        let name = plan
            .current
            .strip_prefix("refs/heads/")
            .ok_or(GitError::MalformedOutput)?;
        let remote = run_git(
            workspace,
            &[
                "config",
                "--local",
                &format!("branch.{name}.remote"),
                &plan.remote,
            ],
        );
        let merge = run_git(
            workspace,
            &[
                "config",
                "--local",
                &format!("branch.{name}.merge"),
                &format!("refs/heads/{}", plan.destination),
            ],
        );
        return Ok(
            remote.is_ok_and(|r| r.status.success()) && merge.is_ok_and(|r| r.status.success())
        );
    }
    Ok(true)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::fs;
    #[test]
    fn deadline_and_normal_exit_stop_helper_descendants() {
        for wait in [true, false] {
            let root = std::env::temp_dir()
                .join(format!("bridge-git-network-{}-{wait}", std::process::id()));
            fs::create_dir_all(&root).unwrap();
            let script = "alias.bridge-network-probe=!sleep 60 & echo $! > helper.pid; ".to_owned()
                + if wait { "wait" } else { "exit 0" };
            let started = Instant::now();
            let result = network_with_timeout(
                &root,
                &["-c", &script, "bridge-network-probe"],
                Duration::from_millis(300),
            );
            if wait {
                assert_eq!(result.unwrap_err(), GitError::Timeout);
            } else {
                assert!(result.is_ok());
            }
            assert!(started.elapsed() < Duration::from_secs(3));
            let pid = fs::read_to_string(root.join("helper.pid")).unwrap();
            let path = format!("/proc/{}/stat", pid.trim());
            let alive = || {
                fs::read_to_string(&path).is_ok_and(|stat| {
                    stat.rsplit_once(") ").is_some_and(|(_, tail)| {
                        !matches!(tail.as_bytes().first(), Some(b'Z' | b'X'))
                    })
                })
            };
            let deadline = Instant::now() + Duration::from_secs(1);
            while alive() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            assert!(!alive(), "helper survived network operation");
            fs::remove_dir_all(root).unwrap();
        }
    }
}
