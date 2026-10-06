use super::LaunchArgs;
use std::{ffi::OsString, path::PathBuf, process::ExitCode};
pub struct Args {
    common: LaunchArgs,
    seconds: u64,
    quarantine: bool,
    worktree: bool,
    purge: bool,
    confirm: Option<String>,
    apply: bool,
    failed: bool,
    vacuum: bool,
}
pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Args, &'static str> {
    let mut args = args.into_iter();
    let (mut project, mut config, mut state, mut age) = (None, None, None, None);
    let mut worktree = false;
    let (mut quarantine, mut purge, mut confirm) = (false, false, None);
    let (mut mode, mut failed, mut vacuum) = (None, false, false);
    while let Some(flag) = args.next() {
        if flag == "--worktree" {
            if worktree {
                return Err("duplicate flag");
            }
            worktree = true;
            continue;
        }
        if flag == "--quarantine" {
            if quarantine {
                return Err("duplicate flag");
            }
            quarantine = true;
            continue;
        }
        if flag == "--purge" {
            if purge {
                return Err("duplicate flag");
            }
            purge = true;
            continue;
        }
        if flag == "--confirm" {
            if confirm.is_some() {
                return Err("duplicate option");
            }
            confirm = Some(
                args.next()
                    .ok_or("confirmation required")?
                    .into_string()
                    .map_err(|_| "invalid confirmation")?,
            );
            continue;
        }
        if flag == "--apply" || flag == "--dry-run" {
            if mode.replace(flag == "--apply").is_some() {
                return Err("select one mode");
            }
            continue;
        }
        if flag == "--include-failed" {
            if failed {
                return Err("duplicate flag");
            }
            failed = true;
            continue;
        }
        if flag == "--vacuum" {
            if vacuum {
                return Err("duplicate flag");
            }
            vacuum = true;
            continue;
        }
        let slot = match flag.to_str() {
            Some("--project") => &mut project,
            Some("--config") => &mut config,
            Some("--state-root") => &mut state,
            Some("--older-than") => &mut age,
            _ => return Err("unsupported prune option"),
        };
        if slot.is_some() {
            return Err("duplicate option");
        }
        *slot = Some(args.next().ok_or("option value required")?);
    }
    let apply = mode.unwrap_or(false);
    if vacuum && !apply {
        return Err("--vacuum requires --apply");
    }
    if quarantine && (age.is_some() || vacuum || failed) {
        return Err("history flags cannot combine with quarantine");
    }
    if !quarantine && (purge || confirm.is_some()) {
        return Err("--purge/--confirm require --quarantine");
    }
    if quarantine && apply && confirm.is_none() {
        return Err("--confirm required for quarantine apply");
    }
    if confirm.is_some() && !apply {
        return Err("--confirm requires --apply");
    }
    let seconds = if quarantine {
        0
    } else {
        bridge_storage::prune::duration_seconds(
            age.ok_or("--older-than required")?
                .to_str()
                .ok_or("invalid duration")?,
        )?
    };
    Ok(Args {
        common: LaunchArgs {
            project: project
                .ok_or("--project required")?
                .into_string()
                .map_err(|_| "invalid project")?,
            config: PathBuf::from(config.ok_or("--config required")?),
            state_root: PathBuf::from(state.ok_or("--state-root required")?),
        },
        seconds,
        quarantine,
        worktree,
        purge,
        confirm,
        apply,
        failed,
        vacuum,
    })
}
pub fn run(args: Args) -> Result<ExitCode, String> {
    let config =
        bridge_config::load_config_with_state_root(&args.common.config, &args.common.state_root)
            .map_err(|e| e.to_string())?;
    let p = config
        .project(&args.common.project)
        .ok_or("project not configured")?;
    let layout = bridge_storage::RustStateLayout::new(args.common.state_root, p.id().clone())
        .map_err(|e| e.to_string())?;
    bridge_runtime::project::validate(p, &layout).map_err(|e| e.to_string())?;
    if args.quarantine {
        let report = if args.apply {
            bridge_worker::quarantine::apply(
                p,
                &layout,
                args.purge,
                args.confirm.as_deref().unwrap(),
            )
        } else {
            bridge_worker::quarantine::preview(p, &layout, args.purge)
        }?;
        println!("{report}");
        return Ok(ExitCode::SUCCESS);
    }
    let mut cleanup_ids = vec![];
    if args.worktree && layout.database().exists() {
        let storage = layout.open_readonly().map_err(|_| "state unowned")?;
        let mut query=storage.connection().prepare("SELECT t.task_id FROM tasks t JOIN worktrees w USING(task_id) WHERE t.project_id=?1 AND (t.status IN ('accepted','closed') OR (?2 AND t.status='failed')) AND julianday(t.updated_at)<julianday('now',?3) AND w.status IN ('created','removing') ORDER BY t.task_id").map_err(|_|"worktree selection failed")?;
        cleanup_ids = query
            .query_map(
                rusqlite::params![
                    p.id().as_str(),
                    args.failed,
                    format!("-{} seconds", args.seconds)
                ],
                |r| r.get::<_, String>(0),
            )
            .map_err(|_| "worktree selection failed")?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| "worktree selection failed")?;
        drop(query);
        drop(storage);
        if args.apply {
            for id in &cleanup_ids {
                bridge_worker::lifecycle::prune_terminal_worktree(
                    &layout,
                    p,
                    id.parse().map_err(|_| "invalid saved task")?,
                    &[&layout],
                )
                .map_err(|e| e.to_string())?;
            }
        }
    }
    let rows =
        bridge_storage::prune::run(&layout, args.seconds, args.apply, args.failed, args.vacuum)?;
    println!(
        "{}",
        serde_json::json!({"dry_run":!args.apply,"count":rows.len(),"worktrees":cleanup_ids,"tasks":rows.iter().map(|r|serde_json::json!({"task_id":r.task_id,"status":r.status})).collect::<Vec<_>>()})
    );
    Ok(ExitCode::SUCCESS)
}
