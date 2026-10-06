//! Byte-preserving, explicit OpenCode config relocation. No provider calls.
use crate::{load_config_with_state_root, parse_project_env, validate_config_text};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
};
pub fn safe_path(path: &Path) -> Result<(), &'static str> {
    let mut prefix = PathBuf::new();
    for c in path.components() {
        if matches!(c, Component::ParentDir) {
            return Err("unsafe path");
        }
        prefix.push(c);
        match fs::symlink_metadata(&prefix) {
            Ok(m) if m.file_type().is_symlink() => return Err("symlink path refused"),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("path unavailable"),
        }
    }
    Ok(())
}
pub fn atomic_write(path: &Path, bytes: &[u8], mode: u32) -> Result<(), &'static str> {
    safe_path(path)?;
    let parent = path.parent().ok_or("parent required")?;
    fs::create_dir_all(parent).map_err(|_| "directory creation failed")?;
    let temp = parent.join(format!(
        ".bridge-edit-{}-{}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| "clock unavailable")?
            .as_nanos()
    ));
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&temp)
        .map_err(|_| "temporary file unavailable")?;
    let result = (|| {
        f.write_all(bytes)
            .and_then(|()| f.sync_all())
            .map_err(|_| "file sync failed")?;
        fs::set_permissions(&temp, fs::Permissions::from_mode(mode))
            .map_err(|_| "mode update failed")?;
        safe_path(path)?;
        fs::rename(&temp, path).map_err(|_| "atomic replacement failed")?;
        fs::File::open(parent)
            .and_then(|f| f.sync_all())
            .map_err(|_| "directory sync failed")
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}
fn read(path: &Path) -> Result<Option<Vec<u8>>, &'static str> {
    safe_path(path)?;
    match fs::metadata(path) {
        Ok(m) if !m.is_file() || m.len() > 16 * 1024 * 1024 => {
            return Err("invalid migration input");
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("migration input unavailable"),
    };
    fs::read(path)
        .map(Some)
        .map_err(|_| "migration input unavailable")
}
fn jsonc(raw: &[u8]) -> Result<(), &'static str> {
    let mut out = raw.to_vec();
    let (mut i, mut string, mut escape) = (0, false, false);
    while i < out.len() {
        let b = out[i];
        if string {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                string = false;
            }
            i += 1;
            continue;
        }
        if b == b'"' {
            string = true;
            i += 1;
            continue;
        }
        if b == b'/' && out.get(i + 1) == Some(&b'/') {
            while i < out.len() && out[i] != b'\n' {
                out[i] = b' ';
                i += 1;
            }
            continue;
        }
        if b == b'/' && out.get(i + 1) == Some(&b'*') {
            out[i] = b' ';
            out[i + 1] = b' ';
            i += 2;
            let mut closed = false;
            while i + 1 < out.len() {
                if out[i] == b'*' && out[i + 1] == b'/' {
                    out[i] = b' ';
                    out[i + 1] = b' ';
                    i += 2;
                    closed = true;
                    break;
                }
                if out[i] != b'\n' {
                    out[i] = b' ';
                }
                i += 1;
            }
            if !closed {
                return Err("invalid JSONC");
            }
            continue;
        }
        i += 1;
    }
    let (mut string, mut escape) = (false, false);
    for i in 0..out.len() {
        let b = out[i];
        if string {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                string = false;
            }
            continue;
        }
        if b == b'"' {
            string = true;
            continue;
        }
        if b == b',' {
            let next = out[i + 1..].iter().find(|b| !b.is_ascii_whitespace());
            if matches!(next, Some(b']' | b'}')) {
                out[i] = b' ';
            }
        }
    }
    let v: Value = serde_json::from_slice(&out).map_err(|_| "invalid JSON/JSONC")?;
    if !v.is_object() {
        return Err("config root must be an object");
    }
    Ok(())
}
pub struct Options {
    pub config: PathBuf,
    pub state_root: PathBuf,
    pub project: String,
    pub source: Option<PathBuf>,
    pub destination: Option<PathBuf>,
    pub force: bool,
}
struct FileChange {
    path: PathBuf,
    original: Option<Vec<u8>>,
    bytes: Vec<u8>,
    mode: u32,
    changed: bool,
}
pub struct Plan {
    files: Vec<FileChange>,
    source: PathBuf,
    source_bytes: Vec<u8>,
}
impl Plan {
    pub fn report(&self) -> Value {
        json!({"changed":self.files.iter().any(|f|f.changed),"files":self.files.iter().map(|f|json!({"path":f.path,"changed":f.changed,"mode":format!("{:04o}",f.mode)})).collect::<Vec<_>>()})
    }
    pub fn apply(&self) -> Result<(), &'static str> {
        use std::os::fd::AsRawFd;
        let parent = self
            .files
            .last()
            .ok_or("empty migration plan")?
            .path
            .parent()
            .ok_or("config directory required")?;
        let handle = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(parent.join(".agent-bridge-config.lock"))
            .map_err(|_| "config lock unavailable")?;
        nix::fcntl::flock(
            handle.as_raw_fd(),
            nix::fcntl::FlockArg::LockExclusiveNonblock,
        )
        .map_err(|_| "config lock busy")?;
        if read(&self.source)?.as_ref() != Some(&self.source_bytes) {
            return Err("source changed; retry preview");
        }
        for f in &self.files {
            if read(&f.path)? != f.original {
                return Err("input changed; retry preview");
            }
            safe_path(&f.path.with_extension(format!(
                "{}.bak",
                f.path.extension().unwrap_or_default().to_string_lossy()
            )))?;
        }
        for f in self.files.iter().filter(|f| f.changed) {
            if let Some(old) = &f.original {
                let backup = PathBuf::from(format!("{}.bak", f.path.to_string_lossy()));
                let mode = fs::metadata(&f.path)
                    .map_err(|_| "input unavailable")?
                    .permissions()
                    .mode()
                    & 0o777;
                atomic_write(&backup, old, mode)?;
            }
            atomic_write(&f.path, &f.bytes, f.mode)?;
        }
        Ok(())
    }
}
pub fn plan(options: &Options) -> Result<Plan, &'static str> {
    safe_path(&options.config)?;
    let config = load_config_with_state_root(&options.config, &options.state_root)
        .map_err(|_| "config invalid")?;
    let project = config
        .project(&options.project)
        .ok_or("project not configured")?;
    let parent = options.config.parent().ok_or("config parent required")?;
    let source = options.source.clone().unwrap_or_else(|| {
        let p = project.workspace().join("opencode.json");
        if p.exists() {
            p
        } else {
            project.workspace().join("opencode.jsonc")
        }
    });
    let dest = options.destination.clone().unwrap_or_else(|| {
        parent
            .join("opencode")
            .join(format!("{}.json", options.project))
    });
    if source
        .file_name()
        .is_some_and(|v| v == "controller-opencode.json")
        || dest
            .file_name()
            .is_some_and(|v| v == "controller-opencode.json")
    {
        return Err("controller config forbidden");
    }
    if !source.is_absolute() || !dest.is_absolute() {
        return Err("absolute migration paths required");
    }
    let env = project
        .opencode_env_file()
        .map(|f| f.as_path().to_owned())
        .unwrap_or_else(|| {
            parent
                .join("secrets")
                .join(format!("{}.env", options.project))
        });
    let paths = [&source, &dest, &env, &options.config];
    let mut seen = BTreeSet::new();
    let mut all = vec![];
    for path in paths {
        safe_path(path)?;
        all.push(path.to_owned());
        if path != &source {
            all.push(PathBuf::from(format!("{}.bak", path.to_string_lossy())));
        }
    }
    for path in &all {
        safe_path(path)?;
        let _ = read(path)?;
        if !seen.insert(path.clone()) {
            return Err("migration paths overlap");
        }
    }
    for a in &all {
        for b in &all {
            if a != b && (a.starts_with(b) || b.starts_with(a)) {
                return Err("migration paths overlap");
            }
        }
    }
    let source_bytes = read(&source)?.ok_or("source unavailable")?;
    jsonc(&source_bytes)?;
    let old_dest = read(&dest)?;
    if old_dest.as_ref().is_some_and(|b| *b != source_bytes) && !options.force {
        return Err("destination differs; --force required");
    }
    let old_env = read(&env)?;
    let env_text = String::from_utf8(old_env.clone().unwrap_or_default())
        .map_err(|_| "env encoding invalid")?;
    parse_project_env(&env_text).map_err(|_| "env invalid")?;
    let mut new_env = String::new();
    let mut replaced = false;
    for line in env_text.split_inclusive('\n') {
        let text = line.trim_end_matches(['\r', '\n']);
        if text
            .split_once('=')
            .is_some_and(|(k, _)| k.trim() == "OPENCODE_CONFIG")
        {
            new_env.push_str(&format!(
                "OPENCODE_CONFIG={}\n",
                dest.to_str().ok_or("destination encoding invalid")?
            ));
            replaced = true;
        } else {
            new_env.push_str(line);
        }
    }
    if !replaced {
        if !new_env.is_empty() && !new_env.ends_with('\n') {
            new_env.push('\n');
        }
        new_env.push_str(&format!(
            "OPENCODE_CONFIG={}\n",
            dest.to_str().ok_or("destination encoding invalid")?
        ));
    }
    parse_project_env(&new_env).map_err(|_| "resulting env invalid")?;
    let old_config = read(&options.config)?.ok_or("config unavailable")?;
    let text = std::str::from_utf8(&old_config).map_err(|_| "config encoding invalid")?;
    let mut document = text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|_| "config invalid")?;
    if project.opencode_env_file().is_none() {
        document["projects"][&options.project]["opencode_env_file"] =
            toml_edit::value(format!("secrets/{}.env", options.project));
    }
    let new_config = document.to_string().into_bytes();
    validate_config_text(
        std::str::from_utf8(&new_config).map_err(|_| "config invalid")?,
        &options.config,
        Some(&options.state_root),
    )
    .map_err(|_| "resulting config invalid")?;
    let change = |path: PathBuf, original: Option<Vec<u8>>, bytes: Vec<u8>, mode: u32| {
        let changed = original.as_ref() != Some(&bytes)
            || fs::metadata(&path).is_ok_and(|m| m.permissions().mode() & 0o777 != mode);
        FileChange {
            path,
            original,
            bytes,
            mode,
            changed,
        }
    };
    let config_mode = fs::metadata(&options.config)
        .map_err(|_| "config unavailable")?
        .permissions()
        .mode()
        & 0o777;
    Ok(Plan {
        source,
        source_bytes: source_bytes.clone(),
        files: vec![
            change(dest, old_dest, source_bytes, 0o600),
            change(env, old_env, new_env.into_bytes(), 0o600),
            change(
                options.config.clone(),
                Some(old_config),
                new_config,
                config_mode,
            ),
        ],
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn jsonc_validates_comments_strings_and_trailing_commas() {
        jsonc(
            br#"{/* c */ "url":"https://x/", "a":[1,], //line
}"#,
        )
        .unwrap();
        for raw in [b"[]".as_slice(), b"{/*", b"{bad}"] {
            assert!(jsonc(raw).is_err());
        }
    }
}
