use std::{ffi::OsString, path::PathBuf, process::ExitCode};
pub struct Args {
    options: bridge_config::migration::Options,
    apply: bool,
}
pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Args, &'static str> {
    let mut args = args.into_iter();
    let (mut config, mut state, mut project, mut source, mut dest) = (None, None, None, None, None);
    let (mut mode, mut force) = (None, false);
    while let Some(flag) = args.next() {
        if flag == "--apply" || flag == "--dry-run" {
            if mode.replace(flag == "--apply").is_some() {
                return Err("select one mode");
            }
            continue;
        }
        if flag == "--force" {
            if force {
                return Err("duplicate flag");
            }
            force = true;
            continue;
        }
        let slot = match flag.to_str() {
            Some("--config") => &mut config,
            Some("--state-root") => &mut state,
            Some("--project") => &mut project,
            Some("--source") => &mut source,
            Some("--destination") => &mut dest,
            _ => return Err("unsupported migration option"),
        };
        if slot.is_some() {
            return Err("duplicate option");
        }
        *slot = Some(args.next().ok_or("option value required")?);
    }
    Ok(Args {
        options: bridge_config::migration::Options {
            config: PathBuf::from(config.ok_or("--config required")?),
            state_root: PathBuf::from(state.ok_or("--state-root required")?),
            project: project
                .ok_or("--project required")?
                .into_string()
                .map_err(|_| "invalid project")?,
            source: source.map(PathBuf::from),
            destination: dest.map(PathBuf::from),
            force,
        },
        apply: mode.unwrap_or(false),
    })
}
pub fn run(mut args: Args) -> Result<ExitCode, String> {
    args.options.config =
        std::fs::canonicalize(&args.options.config).map_err(|_| "config unavailable")?;
    let plan = bridge_config::migration::plan(&args.options)?;
    println!("{}", plan.report());
    if args.apply {
        plan.apply()?;
        println!("migration applied");
    }
    Ok(ExitCode::SUCCESS)
}
