//! Append-only project registration with read-only planning and explicit apply.
use bridge_config::validate_config_text;
use bridge_domain::ProjectId;
use bridge_storage::RustStateLayout;
use std::{
    collections::BTreeSet,
    ffi::OsString,
    fs::{self, OpenOptions},
    io::Write,
    net::TcpListener,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    process::ExitCode,
    time::{SystemTime, UNIX_EPOCH},
};
pub struct Args {
    workspace: PathBuf,
    id: String,
    config: PathBuf,
    state: PathBuf,
    apply: bool,
}
pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Args, &'static str> {
    let mut args = args.into_iter();
    let workspace = PathBuf::from(args.next().ok_or("workspace required")?);
    let (mut id, mut config, mut state) = (None, None, None);
    let mut mode = None;
    while let Some(flag) = args.next() {
        if flag == "--apply" || flag == "--dry-run" {
            if mode.replace(flag == "--apply").is_some() {
                return Err("select one mode");
            }
            continue;
        }
        let slot = match flag.to_str() {
            Some("--id") => &mut id,
            Some("--config") => &mut config,
            Some("--state-root") => &mut state,
            _ => return Err("unsupported add-project option"),
        };
        if slot.is_some() {
            return Err("duplicate option");
        }
        *slot = Some(args.next().ok_or("option value required")?);
    }
    Ok(Args {
        workspace,
        id: id
            .ok_or("--id required")?
            .into_string()
            .map_err(|_| "invalid id")?,
        config: config.ok_or("--config required")?.into(),
        state: state.ok_or("--state-root required")?.into(),
        apply: mode.unwrap_or(false),
    })
}
fn check_path(path: &Path) -> Result<(), String> {
    let mut prefix = PathBuf::new();
    for c in path.components() {
        if matches!(c, Component::ParentDir) {
            return Err("parent traversal refused".into());
        }
        prefix.push(c);
        match fs::symlink_metadata(&prefix) {
            Ok(m) if m.file_type().is_symlink() => return Err("symlink path refused".into()),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("path unavailable".into()),
        }
    }
    Ok(())
}
fn port(used: &BTreeSet<u16>, start: u16, end: u16) -> Result<u16, String> {
    (start..=end)
        .find(|p| !used.contains(p) && TcpListener::bind(("127.0.0.1", *p)).is_ok())
        .ok_or("no free endpoint port".into())
}
fn write_new(path: &Path, bytes: &[u8], mode: u32) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
        .map_err(|_| "exclusive file creation failed")?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|_| "file write failed".into())
}
pub fn run(args: Args) -> Result<ExitCode, String> {
    args.id
        .parse::<ProjectId>()
        .map_err(|_| "invalid project id")?;
    if !args.workspace.is_absolute() || !args.state.is_absolute() {
        return Err("absolute workspace/state required".into());
    }
    let workspace = fs::canonicalize(&args.workspace).map_err(|_| "workspace unavailable")?;
    if !workspace.is_dir() {
        return Err("workspace must be a directory".into());
    }
    let config = if args.config.is_absolute() {
        args.config
    } else {
        std::env::current_dir()
            .map_err(|_| "cwd unavailable")?
            .join(args.config)
    };
    check_path(&config)?;
    check_path(&args.state)?;
    if config.starts_with(&workspace)
        || args.state.starts_with(&workspace)
        || workspace.starts_with(&args.state)
    {
        return Err("configuration/state must be outside workspace".into());
    }
    let original = match fs::read_to_string(&config) {
        Ok(s) => Some(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => return Err("config unavailable".into()),
    };
    let text = original.as_deref().unwrap_or("");
    let mut used = BTreeSet::new();
    if !text.trim().is_empty() {
        let old =
            validate_config_text(text, &config, Some(&args.state)).map_err(|e| e.to_string())?;
        if let Some(p) = old.project(&args.id) {
            if p.workspace() == workspace {
                println!("already present; no changes");
                return Ok(ExitCode::SUCCESS);
            }
            return Err("project id already bound".into());
        }
        for p in old.projects().values() {
            if p.workspace() == workspace {
                return Err("workspace already bound".into());
            }
            used.insert(p.opencode_endpoint().port());
            if let Some(m) = p.mcp_endpoint() {
                used.insert(m.port());
            }
        }
    }
    let opencode = port(&used, 4101, 4199)?;
    let mcp = port(&used, 4201, 4299)?;
    let quote = |s: &str| serde_json::to_string(s).unwrap();
    let workspace = workspace.to_str().ok_or("workspace encoding unsupported")?;
    let block = format!(
        "\n[projects.{}]\nworkspace = {}\nopencode_url = \"http://127.0.0.1:{opencode}\"\npassword_file = {}\nmax_rounds = 3\nmcp_url = \"http://127.0.0.1:{mcp}/mcp\"\nmcp_token_file = {}\n",
        args.id,
        quote(workspace),
        quote(&format!("secrets/{}.password", args.id)),
        quote(&format!("secrets/{}.mcp-token", args.id))
    );
    let new = if text.is_empty() {
        block.trim_start_matches('\n').to_owned()
    } else {
        format!(
            "{text}{}{block}",
            if text.ends_with('\n') { "" } else { "\n" }
        )
    };
    let proposed =
        validate_config_text(&new, &config, Some(&args.state)).map_err(|e| e.to_string())?;
    let project = proposed.project(&args.id).ok_or("project missing")?;
    let layout =
        RustStateLayout::new(args.state, project.id().clone()).map_err(|e| e.to_string())?;
    bridge_runtime::project::validate(project, &layout).map_err(|e| e.to_string())?;
    println!("--- projects.toml\n+++ projects.toml\n@@ append @@");
    for line in new[text.len()..].lines() {
        println!("+{line}");
    }
    if !args.apply {
        return Ok(ExitCode::SUCCESS);
    }
    let existing = validate_config_text(text, &config, Some(layout.state_root()))
        .map_err(|e| e.to_string())
        .or_else(|e| {
            if original.is_none() || text.trim().is_empty() {
                validate_config_text("[projects]\n", &config, Some(layout.state_root()))
                    .map_err(|e| e.to_string())
            } else {
                Err(e)
            }
        })?;
    let _runtime = bridge_runtime::project::config_edit_guard(&existing, layout.state_root())
        .map_err(|_| "stop tasks, services and controllers before changing configuration")?;
    let parent = config.parent().ok_or("config directory required")?;
    fs::create_dir_all(parent).map_err(|_| "config directory creation failed")?;
    let lock = parent.join(".agent-bridge-config.lock");
    let mut options = OpenOptions::new();
    let handle = options
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(lock)
        .map_err(|_| "config lock unavailable")?;
    use std::os::fd::AsRawFd;
    nix::fcntl::flock(
        handle.as_raw_fd(),
        nix::fcntl::FlockArg::LockExclusiveNonblock,
    )
    .map_err(|_| "config lock busy")?;
    check_path(&config)?;
    let current = fs::read_to_string(&config).ok();
    if current != original {
        return Err("configuration changed; retry preview".into());
    }
    let mode = fs::metadata(&config)
        .map(|m| m.permissions().mode() & 0o777)
        .unwrap_or(0o644);
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "clock unavailable")?
        .as_nanos();
    if let Some(old) = &original {
        let backup = parent.join(format!(
            "{}.{}.bak",
            config
                .file_name()
                .ok_or("config filename required")?
                .to_string_lossy(),
            stamp
        ));
        write_new(&backup, old.as_bytes(), mode)?;
    }
    let temp = parent.join(format!(".bridge-config-{stamp}.tmp"));
    write_new(&temp, new.as_bytes(), mode)?;
    fs::set_permissions(&temp, fs::Permissions::from_mode(mode))
        .map_err(|_| "config mode failed")?;
    let result = fs::rename(&temp, &config);
    if result.is_err() {
        let _ = fs::remove_file(&temp);
        return Err("atomic config write failed".into());
    }
    fs::File::open(parent)
        .and_then(|f| f.sync_all())
        .map_err(|_| "config directory sync failed")?;
    bridge_runtime::project::setup(&[(project.clone(), layout.clone())])
        .map_err(|e| format!("config committed; setup failed: {e}"))?;
    fs::set_permissions(parent.join("secrets"), fs::Permissions::from_mode(0o700))
        .map_err(|_| "config committed; secrets directory mode failed")?;
    fs::set_permissions(layout.project_dir(), fs::Permissions::from_mode(0o700))
        .map_err(|_| "config committed; state directory mode failed")?;
    project
        .read_password()
        .map_err(|_| "config committed; password check failed")?;
    project
        .read_mcp_token()
        .map_err(|_| "config committed; token check failed")?;
    println!("config, credentials and Rust storage: ok");
    Ok(ExitCode::SUCCESS)
}
