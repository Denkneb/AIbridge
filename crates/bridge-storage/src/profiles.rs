//! Read the frozen profile without consulting current project config.
use crate::StorageConnection;
use bridge_domain::{ProfileOrigin, ProfileSnapshot, ProjectId, TaskId};
use rusqlite::{OptionalExtension, params};
use std::{error::Error, fmt};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileReadError {
    MissingTask,
    MissingSnapshot,
    CorruptSnapshot,
    Database,
}
impl ProfileReadError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MissingTask => "unknown_task",
            Self::MissingSnapshot => "profile_snapshot_missing",
            Self::CorruptSnapshot => "profile_snapshot_corrupt",
            Self::Database => "profile_storage_unavailable",
        }
    }
}
impl fmt::Display for ProfileReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
impl Error for ProfileReadError {}
impl StorageConnection {
    /// Project-scoped, read-only integrity check of the entire frozen identity.
    /// NULL profile is legacy; a present profile never falls back to config.
    pub fn get_task_profile(
        &self,
        task: TaskId,
        project: &ProjectId,
    ) -> Result<Option<ProfileSnapshot>, ProfileReadError> {
        self.connection().query_row("SELECT profile,profile_json,profile_hash,profile_source FROM tasks WHERE task_id=?1 AND project_id=?2",params![task.to_string(),project.as_str()],|row| {
            Ok((|| {
                let id:Option<String> = row.get(0).map_err(|_|ProfileReadError::CorruptSnapshot)?;
                let Some(id) = id else { return Ok(None); };
                let raw:Option<String> = row.get(1).map_err(|_|ProfileReadError::CorruptSnapshot)?;
                let raw = raw.filter(|s|!s.is_empty()).ok_or(ProfileReadError::MissingSnapshot)?;
                let snapshot:ProfileSnapshot = serde_json::from_str(&raw).map_err(|_|ProfileReadError::CorruptSnapshot)?;
                let hash:String = row.get(2).map_err(|_|ProfileReadError::CorruptSnapshot)?;
                let source:String = row.get(3).map_err(|_|ProfileReadError::CorruptSnapshot)?;
                let origin = ProfileOrigin::try_from(source).map_err(|_|ProfileReadError::CorruptSnapshot)?;
                snapshot.validate_identity(&id,&hash,origin).map_err(|_|ProfileReadError::CorruptSnapshot)?;
                Ok(Some(snapshot))
            })())
        }).optional().map_err(|_|ProfileReadError::Database)?.ok_or(ProfileReadError::MissingTask)?
    }
}
