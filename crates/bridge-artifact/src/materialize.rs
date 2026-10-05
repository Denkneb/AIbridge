use crate::{DeliveryError, Entry, Result, artifact};
use std::{
    fs,
    os::unix::{
        ffi::OsStringExt,
        fs::{DirBuilderExt, symlink},
    },
    path::Path,
};
fn ensure_parents(root: &Path, path: &str) -> Result<()> {
    artifact::parents(root, path)?;
    let mut current = root.to_path_buf();
    for component in Path::new(path)
        .parent()
        .ok_or(DeliveryError::new("unsafe_artifact_path"))?
        .components()
    {
        current.push(component);
        if !current.exists() {
            fs::DirBuilder::new()
                .mode(0o755)
                .create(&current)
                .map_err(|_| DeliveryError::new("target_unwritable"))?;
            artifact::sync_dir(current.parent().unwrap())?;
        }
    }
    Ok(())
}
pub fn execute(artifact_dir: &Path, workspace: &Path, entry: &Entry) -> Result<()> {
    artifact::parents(workspace, &entry.path)?;
    let target = workspace.join(&entry.path);
    if entry.op == "delete" {
        fs::remove_file(&target).map_err(|_| DeliveryError::new("target_unwritable"))?;
        return artifact::sync_dir(target.parent().unwrap());
    }
    let hash = entry
        .blob_sha256
        .as_deref()
        .ok_or(DeliveryError::new("artifact_corrupt"))?;
    let data = artifact::read_file(&artifact_dir.join("blobs").join(hash))?;
    if artifact::digest(&data) != hash || Some(data.len() as u64) != entry.size {
        return Err(DeliveryError::new("artifact_corrupt"));
    }
    ensure_parents(workspace, &entry.path)?;
    if entry.kind == "regular" {
        artifact::atomic_write(&target, &data, entry.mode).map_err(|e| {
            DeliveryError::new(if e.code == "fsync_failed" {
                e.code
            } else {
                "target_unwritable"
            })
        })
    } else {
        let parent = target.parent().unwrap();
        let temp = parent.join(format!(".ab-{}", uuid::Uuid::new_v4()));
        let result = (|| {
            symlink(std::ffi::OsString::from_vec(data), &temp)
                .map_err(|_| DeliveryError::new("target_unwritable"))?;
            fs::rename(&temp, &target).map_err(|_| DeliveryError::new("target_unwritable"))?;
            artifact::sync_dir(parent)
        })();
        if result.is_err() {
            let _ = fs::remove_file(temp);
        }
        result
    }
}
