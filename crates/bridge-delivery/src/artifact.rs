use crate::{DeliveryError, Result};
use bridge_domain::TaskId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::Write,
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Component, Path},
};
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub path: String,
    pub op: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub mode: u32,
    pub blob_sha256: Option<String>,
    pub size: Option<u64>,
    pub base_type: Option<String>,
    pub base_mode: Option<u32>,
    pub base_sha256: Option<String>,
}
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub version: u32,
    pub task_id: TaskId,
    pub base_head: String,
    pub entries: Vec<Entry>,
}
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Object {
    pub kind: String,
    pub mode: u32,
    pub data: Vec<u8>,
}
pub(crate) fn digest(data: &[u8]) -> String {
    format!("{:x}", Sha256::digest(data))
}
fn hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub(crate) fn safe_path(path: &str) -> Result<()> {
    if path.is_empty()
        || path.contains(['\0', '\\'])
        || Path::new(path).is_absolute()
        || path
            .split('/')
            .any(|s| s.is_empty() || s == "." || s == ".." || s.eq_ignore_ascii_case(".git"))
    {
        return Err(DeliveryError::new("unsafe_artifact_path"));
    }
    Ok(())
}
pub(crate) fn parents(root: &Path, path: &str) -> Result<()> {
    safe_path(path)?;
    let parent = Path::new(path)
        .parent()
        .ok_or(DeliveryError::new("unsafe_artifact_path"))?;
    let mut current = root.to_path_buf();
    for part in parent.components() {
        if let Component::Normal(p) = part {
            current.push(p);
            match fs::symlink_metadata(&current) {
                Ok(m) if m.is_dir() && !m.file_type().is_symlink() => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                _ => return Err(DeliveryError::new("unsafe_artifact_path")),
            }
        } else {
            return Err(DeliveryError::new("unsafe_artifact_path"));
        }
    }
    Ok(())
}
pub(crate) fn read_object(root: &Path, path: &str) -> Result<Option<Object>> {
    parents(root, path)?;
    let target = root.join(path);
    let meta = match fs::symlink_metadata(&target) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(DeliveryError::new("target_unreadable")),
    };
    if meta.file_type().is_symlink() {
        return Ok(Some(Object {
            kind: "symlink".into(),
            mode: 0o120000,
            data: fs::read_link(target)
                .map_err(|_| DeliveryError::new("target_unreadable"))?
                .as_os_str()
                .as_bytes()
                .to_vec(),
        }));
    }
    if !meta.is_file() {
        return Err(DeliveryError::new("unsupported_object_type"));
    }
    let mut file = std::io::BufReader::new(
        OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(target)
            .map_err(|_| DeliveryError::new("target_unreadable"))?,
    );
    use std::io::Read;
    let mut data = vec![];
    file.read_to_end(&mut data)
        .map_err(|_| DeliveryError::new("target_unreadable"))?;
    Ok(Some(Object {
        kind: "regular".into(),
        mode: if meta.mode() & 0o111 != 0 {
            0o100755
        } else {
            0o100644
        },
        data,
    }))
}
impl Entry {
    pub(crate) fn matches_base(&self, obj: Option<&Object>) -> bool {
        if self.op == "add" {
            return obj.is_none();
        }
        obj.is_some_and(|o| {
            Some(&o.kind) == self.base_type.as_ref()
                && Some(o.mode) == self.base_mode
                && Some(digest(&o.data)) == self.base_sha256
        })
    }
    pub(crate) fn matches_artifact(&self, obj: Option<&Object>) -> bool {
        if self.op == "delete" {
            return obj.is_none();
        }
        obj.is_some_and(|o| {
            o.kind == self.kind
                && o.mode == self.mode
                && Some(digest(&o.data)) == self.blob_sha256
                && Some(o.data.len() as u64) == self.size
        })
    }
    fn validate(&self) -> Result<()> {
        safe_path(&self.path)?;
        let type_mode = |kind: &str, mode: u32| {
            matches!(
                (kind, mode),
                ("regular", 0o100644 | 0o100755) | ("symlink", 0o120000)
            )
        };
        let valid_digest = |s: &Option<String>| s.as_deref().is_some_and(|s| hex(s, 64));
        if !matches!(
            self.op.as_str(),
            "add" | "modify" | "delete" | "mode_change"
        ) || !type_mode(&self.kind, self.mode)
        {
            return Err(DeliveryError::new("artifact_corrupt"));
        }
        if self.op == "delete" {
            if self.blob_sha256.is_some() || self.size.is_some() {
                return Err(DeliveryError::new("artifact_corrupt"));
            }
        } else if !valid_digest(&self.blob_sha256) || self.size.is_none() {
            return Err(DeliveryError::new("artifact_corrupt"));
        }
        if self.op == "add" {
            if self.base_type.is_some() || self.base_mode.is_some() || self.base_sha256.is_some() {
                return Err(DeliveryError::new("artifact_corrupt"));
            }
        } else if !self
            .base_type
            .as_deref()
            .zip(self.base_mode)
            .is_some_and(|(t, m)| type_mode(t, m))
            || !valid_digest(&self.base_sha256)
        {
            return Err(DeliveryError::new("artifact_corrupt"));
        }
        Ok(())
    }
}
type Blobs = BTreeMap<String, Vec<u8>>;
pub fn build_entries(
    checkout: &Path,
    task: TaskId,
    base: &str,
    baseline: &bridge_git::RepositorySnapshot,
    scopes: &[String],
) -> Result<(Artifact, Blobs)> {
    let comparison =
        bridge_git::compare_repository_snapshot(checkout, baseline, scopes, true, false)
            .map_err(|_| DeliveryError::new("snapshot_failed"))?;
    if !comparison.scope_violations().is_empty() {
        return Err(DeliveryError::paths(
            "out_of_scope_changes",
            comparison
                .scope_violations()
                .iter()
                .filter_map(|s| s.to_str().map(str::to_owned))
                .collect(),
        ));
    }
    let mut entries = vec![];
    let mut blobs = BTreeMap::new();
    for path in comparison.changed_paths() {
        let path = path
            .to_str()
            .ok_or(DeliveryError::new("unsupported_path_encoding"))?;
        safe_path(path)?;
        let before = bridge_git::objects::commit_object(checkout, base, path)
            .map_err(|_| DeliveryError::new("base_object_unreadable"))?
            .map(|o| Object {
                kind: if o.mode == 0o120000 {
                    "symlink".into()
                } else {
                    "regular".into()
                },
                mode: o.mode,
                data: o.data,
            });
        let after = read_object(checkout, path)?;
        if before == after {
            continue;
        }
        let op = match (&before, &after) {
            (None, Some(_)) => "add",
            (Some(_), None) => "delete",
            (Some(b), Some(a)) if b.kind == a.kind && b.data == a.data => "mode_change",
            (Some(_), Some(_)) => "modify",
            _ => continue,
        };
        let obj = after.as_ref().or(before.as_ref()).unwrap();
        let sha = after.as_ref().map(|o| digest(&o.data));
        if let Some(sha) = &sha {
            blobs.insert(sha.clone(), after.as_ref().unwrap().data.clone());
        }
        entries.push(Entry {
            path: path.into(),
            op: op.into(),
            kind: obj.kind.clone(),
            mode: obj.mode,
            blob_sha256: sha,
            size: after.as_ref().map(|o| o.data.len() as u64),
            base_type: before.as_ref().map(|o| o.kind.clone()),
            base_mode: before.as_ref().map(|o| o.mode),
            base_sha256: before.as_ref().map(|o| digest(&o.data)),
        });
    }
    Ok((
        Artifact {
            version: 1,
            task_id: task,
            base_head: base.into(),
            entries,
        },
        blobs,
    ))
}
pub(crate) fn private_dir(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() && !m.file_type().is_symlink() => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::os::unix::fs::DirBuilderExt::mode(&mut fs::DirBuilder::new(), 0o700)
                .create(path)
                .map_err(|_| DeliveryError::new("artifact_unwritable"))?;
            sync_dir(
                path.parent()
                    .ok_or(DeliveryError::new("artifact_unwritable"))?,
            )?;
        }
        _ => return Err(DeliveryError::new("artifact_unwritable")),
    };
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|_| DeliveryError::new("artifact_unwritable"))
}
pub(crate) fn sync_dir(path: &Path) -> Result<()> {
    std::fs::File::open(path)
        .and_then(|f| f.sync_all())
        .map_err(|_| DeliveryError::new("fsync_failed"))
}
pub(crate) fn atomic_write(path: &Path, data: &[u8], mode: u32) -> Result<()> {
    let parent = path
        .parent()
        .ok_or(DeliveryError::new("artifact_unwritable"))?;
    let temp = parent.join(format!(".ab-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(&temp)
            .map_err(|_| DeliveryError::new("artifact_unwritable"))?;
        file.write_all(data)
            .map_err(|_| DeliveryError::new("artifact_unwritable"))?;
        file.set_permissions(fs::Permissions::from_mode(mode & 0o777))
            .map_err(|_| DeliveryError::new("artifact_unwritable"))?;
        file.sync_all()
            .map_err(|_| DeliveryError::new("fsync_failed"))?;
        fs::rename(&temp, path).map_err(|_| DeliveryError::new("artifact_unwritable"))?;
        sync_dir(parent)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}
pub(crate) fn write_artifact(dest: &Path, artifact: &Artifact, blobs: &Blobs) -> Result<()> {
    private_dir(dest)?;
    private_dir(&dest.join("blobs"))?;
    for (hash, bytes) in blobs {
        if digest(bytes) != *hash {
            return Err(DeliveryError::new("artifact_corrupt"));
        }
        atomic_write(&dest.join("blobs").join(hash), bytes, 0o600)?;
    }
    atomic_write(
        &dest.join("manifest.json"),
        &serde_json::to_vec(artifact).map_err(|_| DeliveryError::new("artifact_corrupt"))?,
        0o600,
    )
}
pub(crate) fn read_file(path: &Path) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut f = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(path)
        .map_err(|_| DeliveryError::new("artifact_corrupt"))?;
    if !f
        .metadata()
        .map_err(|_| DeliveryError::new("artifact_corrupt"))?
        .is_file()
    {
        return Err(DeliveryError::new("artifact_corrupt"));
    }
    let mut data = vec![];
    f.read_to_end(&mut data)
        .map_err(|_| DeliveryError::new("artifact_corrupt"))?;
    Ok(data)
}
pub fn load_artifact(dest: &Path) -> Result<Artifact> {
    for dir in [dest.to_path_buf(), dest.join("blobs")] {
        if !fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink()) {
            return Err(DeliveryError::new("artifact_corrupt"));
        }
    }
    let artifact: Artifact = serde_json::from_slice(&read_file(&dest.join("manifest.json"))?)
        .map_err(|_| DeliveryError::new("artifact_corrupt"))?;
    if artifact.version != 1 || !(hex(&artifact.base_head, 40) || hex(&artifact.base_head, 64)) {
        return Err(DeliveryError::new("artifact_corrupt"));
    }
    let mut paths = BTreeSet::new();
    for e in &artifact.entries {
        e.validate()?;
        if !paths.insert(&e.path) {
            return Err(DeliveryError::new("artifact_corrupt"));
        }
        if let Some(hash) = &e.blob_sha256 {
            let data = read_file(&dest.join("blobs").join(hash))?;
            if digest(&data) != *hash || Some(data.len() as u64) != e.size {
                return Err(DeliveryError::new("artifact_corrupt"));
            }
        }
    }
    Ok(artifact)
}
