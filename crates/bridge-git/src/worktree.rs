//! Deterministic worktree manifest and stable `worktree_fingerprint` (4.7b).
//!
//! This module mirrors the observable semantics of the reference Python
//! `git_snapshot.manifest` / `git_snapshot._hash_file` and the compact digest
//! serialization of `verifier.fingerprint`, with one deliberate fail-closed
//! extension described below. Entries are ordered exactly like the reference:
//! Python sorts surrogateescape-decoded `str` names by code point, so a valid
//! supplementary code point (for example `U+1F600`) sorts *after* a lone
//! invalid byte such as `0xFF`, which surrogateescape maps to `U+DCFF`.
//! Raw-byte order would place the supplementary code point first and therefore
//! change the fingerprint. The raw path bytes are still retained as the
//! manifest key and hash material.
//!
//! The manifest lists exactly the files Git considers relevant:
//!
//! - tracked files from `git ls-files -z`;
//! - non-ignored untracked files from `git ls-files --others
//!   --exclude-standard -z`.
//!
//! Ignored entries are therefore excluded by Git itself. Paths are carried as
//! raw Unix [`OsString`] bytes and are never decoded lossily, so a non-UTF-8
//! path survives unchanged. Entries are ordered by their surrogateescape code
//! point sequence and deduplicated, so the manifest is deterministic and
//! matches Python's `sorted()` for every byte sequence.
//!
//! Each entry digest captures the file type, the executable bit and, for a
//! symlink, the exact target string (never the content of the target file):
//!
//! - regular file: `sha256(b"file\0" || content)`, or `b"exec\0"` when any
//!   executable bit is set;
//! - symlink: `sha256(b"link\0" || raw_target_bytes)`.
//!
//! A listed path that is absent from the worktree (a deleted tracked file)
//! contributes no entry, matching the reference and the `tracked-deletion`
//! corpus case.
//!
//! Fail-closed extension: unlike the reference, which silently drops an entry
//! when `lstat`/read fails or the entry is a directory, this module reports a
//! payload-free [`GitError::ManifestIo`] on a filesystem error and
//! [`GitError::UnsupportedFileType`] for any listed entry that is neither a
//! regular file nor a symlink (directory, FIFO, socket, device, ...). Malformed
//! `ls-files -z` framing is reported as [`GitError::MalformedOutput`]. No error
//! carries a path, argv, output, Git configuration, OS error text or secret.
//!
//! The stable [`WorktreeFingerprint`] is the reference compact digest:
//!
//! ```text
//! sha256( for each entry in sorted manifest order:
//!             path_bytes || 0x00 || lowercase_hex(entry_digest) || 0x00
//!         || b"status\0" || raw_status_bytes )
//! ```

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::Path;

use crate::sha256::{self, Sha256};
use crate::{GitError, os_from_bytes, run_checked, status_porcelain};

/// One manifest entry: a repository-relative path and its SHA-256 digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestEntry {
    path: OsString,
    digest: [u8; 32],
}

impl ManifestEntry {
    /// Returns the repository-relative path as raw Unix bytes.
    #[must_use]
    pub fn path(&self) -> &OsStr {
        &self.path
    }

    /// Returns the raw 32-byte entry digest.
    #[must_use]
    pub fn digest(&self) -> &[u8; 32] {
        &self.digest
    }

    /// Returns the entry digest as lowercase hexadecimal.
    #[must_use]
    pub fn digest_hex(&self) -> String {
        sha256::to_hex(&self.digest)
    }
}

/// A deterministic manifest of the relevant worktree files.
///
/// Entries are ordered by the reference surrogateescape code point sequence and
/// deduplicated, so two manifests over the same worktree state are always
/// equal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeManifest {
    entries: Vec<ManifestEntry>,
}

impl WorktreeManifest {
    pub(crate) fn from_json(value: &serde_json::Value) -> Result<Self, crate::GitError> {
        let object = value.as_object().ok_or(crate::GitError::MalformedOutput)?;
        let mut entries = Vec::new();
        for (path, digest) in object {
            bridge_domain::CheckpointPath::try_from(path.clone())
                .map_err(|_| crate::GitError::MalformedOutput)?;
            let digest = super::parse_digest(digest)?;
            entries.push(ManifestEntry {
                path: path.into(),
                digest,
            });
        }
        entries.sort_by_key(|e| surrogateescape_key(&e.path));
        Ok(Self { entries })
    }
    /// Returns the entries in deterministic reference (Python `sorted`) order.
    #[must_use]
    pub fn entries(&self) -> &[ManifestEntry] {
        &self.entries
    }

    /// Returns the number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns whether the manifest has no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns the digest for `path`, if present.
    #[must_use]
    pub fn digest(&self, path: &OsStr) -> Option<&[u8; 32]> {
        self.entries
            .iter()
            .find(|entry| entry.path.as_os_str() == path)
            .map(|entry| &entry.digest)
    }
}

/// The stable SHA-256 fingerprint of a worktree manifest plus status.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct WorktreeFingerprint([u8; 32]);

impl WorktreeFingerprint {
    /// Returns the raw 32-byte digest.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Returns the digest as lowercase hexadecimal.
    #[must_use]
    pub fn to_hex(&self) -> String {
        sha256::to_hex(&self.0)
    }
}

impl std::fmt::Debug for WorktreeFingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "WorktreeFingerprint({})", self.to_hex())
    }
}

impl std::fmt::Display for WorktreeFingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// Builds the deterministic worktree manifest of `workspace`.
///
/// # Errors
///
/// Returns the bounded-runner infrastructure errors and
/// [`GitError::CommandFailed`] when a listing exits non-zero,
/// [`GitError::MalformedOutput`] for malformed `-z` framing,
/// [`GitError::ManifestIo`] on a filesystem error and
/// [`GitError::UnsupportedFileType`] for a listed entry that is neither a
/// regular file nor a symlink.
pub fn worktree_manifest(workspace: &Path) -> Result<WorktreeManifest, GitError> {
    let tracked = run_checked(workspace, &["ls-files", "-z"])?;
    let untracked = run_checked(
        workspace,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )?;

    let mut names: BTreeMap<Vec<u32>, OsString> = BTreeMap::new();
    for name in parse_null_listing(&tracked)?
        .into_iter()
        .chain(parse_null_listing(&untracked)?)
    {
        names.insert(surrogateescape_key(&name), name);
    }

    let mut entries = Vec::with_capacity(names.len());
    for name in names.into_values() {
        if let Some(digest) = hash_listed_file(&workspace.join(&name))? {
            entries.push(ManifestEntry { path: name, digest });
        }
    }
    Ok(WorktreeManifest { entries })
}

/// Computes the stable worktree fingerprint of `workspace`.
///
/// # Errors
///
/// Returns the same errors as [`worktree_manifest`] plus the errors of
/// [`status_porcelain`].
pub fn worktree_fingerprint(workspace: &Path) -> Result<WorktreeFingerprint, GitError> {
    let status = status_porcelain(workspace)?;
    let manifest = worktree_manifest(workspace)?;
    Ok(fingerprint_from(&manifest, &status))
}

/// Computes the fingerprint from an already-built manifest and raw status.
pub(crate) fn fingerprint_from(manifest: &WorktreeManifest, status: &[u8]) -> WorktreeFingerprint {
    let mut hasher = Sha256::new();
    for entry in &manifest.entries {
        update_os(&mut hasher, entry.path.as_os_str());
        hasher.update(&[0]);
        hasher.update(entry.digest_hex().as_bytes());
        hasher.update(&[0]);
    }
    hasher.update(b"status\0");
    hasher.update(status);
    WorktreeFingerprint(hasher.finalize())
}

/// Splits NUL-terminated `git ls-files -z` output into raw path names.
fn parse_null_listing(bytes: &[u8]) -> Result<Vec<OsString>, GitError> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let body = bytes.strip_suffix(b"\0").ok_or(GitError::MalformedOutput)?;
    let mut names = Vec::new();
    for field in body.split(|&byte| byte == 0) {
        if field.is_empty() {
            return Err(GitError::MalformedOutput);
        }
        names.push(os_from_bytes(field));
    }
    Ok(names)
}

/// Builds the Python-equivalent `decode("utf-8", "surrogateescape")` ordering
/// key for a path.
///
/// The reference sorts manifest names as Python `str`, i.e. by code point, so a
/// valid supplementary code point such as `U+1F600` sorts *after* a lone
/// invalid byte such as `0xFF` (surrogateescape maps it to `U+DCFF`). Raw-byte
/// order would place `U+1F600` first and change the fingerprint; the raw path
/// bytes remain the manifest key and hash material.
#[cfg(unix)]
pub(crate) fn surrogateescape_key(value: &OsStr) -> Vec<u32> {
    use std::os::unix::ffi::OsStrExt;
    surrogateescape_key_bytes(value.as_bytes())
}

/// Builds the ordering key on non-Unix platforms.
#[cfg(not(unix))]
pub(crate) fn surrogateescape_key(value: &OsStr) -> Vec<u32> {
    surrogateescape_key_bytes(value.to_string_lossy().as_bytes())
}

/// Decodes raw bytes into the surrogateescape code point sequence.
///
/// Valid UTF-8 sequences contribute their code point; every byte that does not
/// begin a valid sequence contributes `U+DC00 | byte`, matching CPython's
/// surrogateescape error handler. Lexicographic comparison of these sequences
/// reproduces Python's `sorted()` over the decoded names.
fn surrogateescape_key_bytes(bytes: &[u8]) -> Vec<u32> {
    let mut key = Vec::with_capacity(bytes.len());
    let mut rest = bytes;
    loop {
        match std::str::from_utf8(rest) {
            Ok(text) => {
                key.extend(text.chars().map(u32::from));
                return key;
            }
            Err(error) => {
                let valid = error.valid_up_to();
                if valid > 0 {
                    let text = std::str::from_utf8(&rest[..valid])
                        .expect("valid_up_to marks a valid UTF-8 prefix");
                    key.extend(text.chars().map(u32::from));
                    rest = &rest[valid..];
                }
                key.push(0xDC00 | u32::from(rest[0]));
                rest = &rest[1..];
            }
        }
    }
}

/// Hashes one listed path, or returns `None` when it is absent.
fn hash_listed_file(path: &Path) -> Result<Option<[u8; 32]>, GitError> {
    Ok(listed_file_state(path)?.map(|(digest, _)| digest))
}

pub(crate) fn listed_file_state(path: &Path) -> Result<Option<([u8; 32], &'static str)>, GitError> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(GitError::ManifestIo),
    };

    let file_type = metadata.file_type();
    let mut hasher = Sha256::new();
    let mut kind = "file";
    if file_type.is_symlink() {
        kind = "symlink";
        let target = std::fs::read_link(path).map_err(|_| GitError::ManifestIo)?;
        hasher.update(b"link\0");
        update_os(&mut hasher, target.as_os_str());
    } else if file_type.is_file() {
        hasher.update(if is_executable(&metadata) {
            b"exec\0"
        } else {
            b"file\0"
        });
        let mut file = std::fs::File::open(path).map_err(|_| GitError::ManifestIo)?;
        let mut buffer = [0u8; 8192];
        loop {
            let read = file.read(&mut buffer).map_err(|_| GitError::ManifestIo)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            if buffer[..read].contains(&0) {
                kind = "binary";
            }
        }
    } else {
        return Err(GitError::UnsupportedFileType);
    }
    Ok(Some((hasher.finalize(), kind)))
}

/// Returns whether any executable bit is set.
#[cfg(unix)]
fn is_executable(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

/// Returns whether any executable bit is set.
#[cfg(not(unix))]
fn is_executable(_metadata: &std::fs::Metadata) -> bool {
    false
}

/// Absorbs the exact bytes of an [`OsStr`] into the hasher.
#[cfg(unix)]
fn update_os(hasher: &mut Sha256, value: &OsStr) {
    use std::os::unix::ffi::OsStrExt;
    hasher.update(value.as_bytes());
}

/// Absorbs the bytes of an [`OsStr`] into the hasher.
#[cfg(not(unix))]
fn update_os(hasher: &mut Sha256, value: &OsStr) {
    hasher.update(value.to_string_lossy().as_bytes());
}
