//! Logical worktree and quarantine registries. No filesystem or process operations.
use super::{StorageConnection, open_read_only_current, utc_now_rfc3339_millis};
use bridge_domain::{ProjectId, TaskId, WorkflowId};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::{error::Error, fmt, num::NonZeroU16, path::Path, str::FromStr};

/// Safe registry failures; database details are exposed only through `source`.
#[non_exhaustive]
pub enum WorktreeStorageError {
    InvalidInput,
    MissingOwner,
    MissingRecord,
    InvalidTransition,
    Drift,
    IncompatibleSchema,
    Database(rusqlite::Error),
}
impl fmt::Display for WorktreeStorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidInput => "invalid worktree registry input",
            Self::MissingOwner => "worktree owner is missing or incompatible",
            Self::MissingRecord => "registry record is missing",
            Self::InvalidTransition => "invalid registry transition",
            Self::Drift => "quarantine registry changed",
            Self::IncompatibleSchema => "incompatible registry schema",
            Self::Database(_) => "worktree registry storage failed",
        })
    }
}
impl fmt::Debug for WorktreeStorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}
impl Error for WorktreeStorageError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        if let Self::Database(e) = self {
            Some(e)
        } else {
            None
        }
    }
}
impl From<rusqlite::Error> for WorktreeStorageError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Database(e)
    }
}
macro_rules! states {
    ($name:ident {$($variant:ident => $value:literal),+} [$($from:ident => $to:ident),*]) => {
        #[derive(Debug,Clone,Copy,PartialEq,Eq)] pub enum $name {$($variant),+}
        impl $name {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];
            pub fn as_str(self)-> &'static str {match self {$(Self::$variant=>$value),+}}
            pub fn allows(self,target:Self)->bool {self==target || matches!((self,target),$((Self::$from,Self::$to))|*)}
        }
        impl FromStr for $name {type Err=WorktreeStorageError; fn from_str(s:&str)->Result<Self,Self::Err>{match s {$($value=>Ok(Self::$variant)),+, _=>Err(WorktreeStorageError::InvalidInput)}}}
    }
}
states!(WorktreeStatus {Pending=>"pending",Creating=>"creating",Created=>"created",Removing=>"removing",Removed=>"removed",Error=>"error"} [Pending=>Creating,Pending=>Error,Pending=>Removed,Creating=>Created,Creating=>Error,Creating=>Removing,Created=>Removing,Removing=>Removed]);
states!(WorktreeDeliveryState {None=>"none",Applying=>"applying",Delivered=>"delivered"} [None=>Applying,Applying=>Delivered]);
states!(WorktreeQuarantineStatus {Quarantined=>"quarantined",Moved=>"moved",Removed=>"removed"} [Quarantined=>Moved,Quarantined=>Removed,Moved=>Removed]);

/// A strictly mapped record; Debug deliberately omits opaque metadata.
#[derive(Clone, PartialEq, Eq)]
pub struct WorktreeRecord {
    pub task_id: TaskId,
    pub path: String,
    pub runtime_dir: Option<String>,
    pub base_head: Option<String>,
    pub baseline_json: Option<String>,
    pub server_endpoint: Option<String>,
    pub server_port: Option<NonZeroU16>,
    pub server_process_record: Option<String>,
    pub status: WorktreeStatus,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub removed_at: Option<String>,
    pub cleanup_reason: Option<String>,
    pub delivery_state: Option<WorktreeDeliveryState>,
    pub delivery_journal_path: Option<String>,
    pub delivered_at: Option<String>,
}
impl fmt::Debug for WorktreeRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WorktreeRecord { .. }")
    }
}
/// Optional registration metadata; paths and baseline are opaque, never acted upon.
#[derive(Default)]
pub struct WorktreeRegistration {
    pub runtime_dir: Option<String>,
    pub base_head: Option<String>,
    pub baseline_json: Option<String>,
    pub server_endpoint: Option<String>,
    pub server_port: Option<NonZeroU16>,
    pub server_process_record: Option<String>,
    pub status: Option<WorktreeStatus>,
    pub delivery_state: Option<WorktreeDeliveryState>,
    pub cleanup_reason: Option<String>,
}
/// Logical quarantine entry. WorkflowId supplies the shared safe-token contract;
/// the entry is independent of workflow/task ownership.
#[derive(Clone, PartialEq, Eq)]
pub struct WorktreeQuarantineEntry {
    pub entry_id: WorkflowId,
    pub original_path: String,
    pub quarantined_path: Option<String>,
    pub reason: Option<String>,
    pub found_at: Option<String>,
    pub status: WorktreeQuarantineStatus,
}
impl fmt::Debug for WorktreeQuarantineEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WorktreeQuarantineEntry { .. }")
    }
}
#[derive(Default)]
pub struct WorktreeQuarantineRegistration {
    pub quarantined_path: Option<String>,
    pub reason: Option<String>,
    pub found_at: Option<String>,
    pub status: Option<WorktreeQuarantineStatus>,
}
const WT: &str = "task_id,path,runtime_dir,base_head,baseline_json,server_endpoint,server_port,server_process_record,status,created_at,updated_at,removed_at,cleanup_reason,delivery_state,delivery_journal_path,delivered_at";
const QT: &str = "entry_id,original_path,quarantined_path,reason,found_at,status";
fn bad_row() -> rusqlite::Error {
    rusqlite::Error::InvalidQuery
}
fn worktree_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<WorktreeRecord> {
    let id: String = r.get(0)?;
    let status: String = r.get(8)?;
    let delivery: Option<String> = r.get(13)?;
    let port: Option<i64> = r.get(6)?;
    let port = port
        .map(|p| {
            u16::try_from(p)
                .ok()
                .and_then(NonZeroU16::new)
                .ok_or_else(bad_row)
        })
        .transpose()?;
    let path: String = r.get(1)?;
    if path.is_empty() {
        return Err(bad_row());
    }
    Ok(WorktreeRecord {
        task_id: id.parse().map_err(|_| bad_row())?,
        path,
        runtime_dir: r.get(2)?,
        base_head: r.get(3)?,
        baseline_json: r.get(4)?,
        server_endpoint: r.get(5)?,
        server_port: port,
        server_process_record: r.get(7)?,
        status: status.parse().map_err(|_| bad_row())?,
        created_at: r.get(9)?,
        updated_at: r.get(10)?,
        removed_at: r.get(11)?,
        cleanup_reason: r.get(12)?,
        delivery_state: delivery
            .filter(|s| !s.is_empty())
            .map(|s| s.parse().map_err(|_| bad_row()))
            .transpose()?,
        delivery_journal_path: r.get(14)?,
        delivered_at: r.get(15)?,
    })
}
fn quarantine_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<WorktreeQuarantineEntry> {
    let id: String = r.get(0)?;
    let status: String = r.get(5)?;
    let original_path: String = r.get(1)?;
    if original_path.is_empty() {
        return Err(bad_row());
    }
    Ok(WorktreeQuarantineEntry {
        entry_id: WorkflowId::try_from(id).map_err(|_| bad_row())?,
        original_path,
        quarantined_path: r.get(2)?,
        reason: r.get(3)?,
        found_at: r.get(4)?,
        status: status.parse().map_err(|_| bad_row())?,
    })
}
fn owner(c: &Connection, id: TaskId, project: &ProjectId) -> Result<(), WorktreeStorageError> {
    let valid:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM tasks WHERE task_id=?1 AND project_id=?2 AND execution_mode='worktree')",params![id.to_string(),project.as_str()],|r|r.get(0))?;
    if valid {
        Ok(())
    } else {
        Err(WorktreeStorageError::MissingOwner)
    }
}
fn wt(c: &Connection, id: TaskId) -> Result<Option<WorktreeRecord>, WorktreeStorageError> {
    Ok(c.query_row(
        &format!("SELECT {WT} FROM worktrees WHERE task_id=?1"),
        [id.to_string()],
        worktree_row,
    )
    .optional()?)
}
fn qt(
    c: &Connection,
    id: &WorkflowId,
) -> Result<Option<WorktreeQuarantineEntry>, WorktreeStorageError> {
    Ok(c.query_row(
        &format!("SELECT {QT} FROM worktree_quarantine WHERE entry_id=?1"),
        [id.as_str()],
        quarantine_row,
    )
    .optional()?)
}
fn event(
    c: &Connection,
    id: TaskId,
    kind: &str,
    message: &str,
    now: &str,
) -> Result<(), WorktreeStorageError> {
    c.execute("INSERT INTO events(task_id,round_number,kind,message,created_at) VALUES (?1,NULL,?2,?3,?4)",params![id.to_string(),kind,message,now])?;
    Ok(())
}
impl StorageConnection {
    pub fn register_worktree(
        &mut self,
        id: TaskId,
        project: &ProjectId,
        path: &str,
        input: &WorktreeRegistration,
    ) -> Result<WorktreeRecord, WorktreeStorageError> {
        if path.is_empty() {
            return Err(WorktreeStorageError::InvalidInput);
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        owner(&tx, id, project)?;
        let now = utc_now_rfc3339_millis();
        tx.execute("INSERT INTO worktrees(task_id,path,runtime_dir,base_head,baseline_json,server_endpoint,server_port,server_process_record,status,created_at,updated_at,cleanup_reason,delivery_state) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?10,?11,?12)",params![id.to_string(),path,input.runtime_dir,input.base_head,input.baseline_json,input.server_endpoint,input.server_port.map(|p|p.get()),input.server_process_record,input.status.unwrap_or(WorktreeStatus::Pending).as_str(),now,input.cleanup_reason,input.delivery_state.unwrap_or(WorktreeDeliveryState::None).as_str()])?;
        let result = wt(&tx, id)?.ok_or(WorktreeStorageError::MissingRecord)?;
        tx.commit()?;
        Ok(result)
    }
    pub fn get_worktree(
        &self,
        id: TaskId,
        project: &ProjectId,
    ) -> Result<Option<WorktreeRecord>, WorktreeStorageError> {
        owner(&self.connection, id, project)?;
        wt(&self.connection, id)
    }
    pub fn update_worktree_status(
        &mut self,
        id: TaskId,
        project: &ProjectId,
        target: WorktreeStatus,
        reason: Option<&str>,
    ) -> Result<WorktreeRecord, WorktreeStorageError> {
        self.mutate_worktree(id,project,|tx,current,now|{
            if !current.status.allows(target){return Err(WorktreeStorageError::InvalidTransition)}
            if current.status==target{return Ok(())}
            tx.execute("UPDATE worktrees SET status=?1,updated_at=?2,removed_at=CASE WHEN ?1='removed' THEN ?2 ELSE removed_at END,cleanup_reason=COALESCE(?3,cleanup_reason) WHERE task_id=?4",params![target.as_str(),now,reason,id.to_string()])?;
            event(tx,id,&format!("worktree_{}",target.as_str()),&format!("worktree status: {} -> {}",current.status.as_str(),target.as_str()),now)
        })
    }
    /// Persists a delivery transition and completion event atomically.
    /// A repeated state ignores supplied metadata, matching the reference.
    pub fn set_worktree_delivery(
        &mut self,
        id: TaskId,
        project: &ProjectId,
        target: WorktreeDeliveryState,
        journal: Option<&str>,
        delivered_at: Option<&str>,
    ) -> Result<WorktreeRecord, WorktreeStorageError> {
        self.mutate_worktree(id,project,|tx,current,now|{
            let previous=current.delivery_state.unwrap_or(WorktreeDeliveryState::None);
            if !previous.allows(target){return Err(WorktreeStorageError::InvalidTransition)}
            if previous==target{return Ok(())}
            let stamp=delivered_at.or_else(||(target==WorktreeDeliveryState::Delivered).then_some(now));
            tx.execute("UPDATE worktrees SET delivery_state=?1,updated_at=?2,delivery_journal_path=COALESCE(?3,delivery_journal_path),delivered_at=COALESCE(?4,delivered_at) WHERE task_id=?5",params![target.as_str(),now,journal,stamp,id.to_string()])?;
            if target==WorktreeDeliveryState::Delivered {event(tx,id,"delivered","delivery completed",now)?} Ok(())
        })
    }
    /// Records server metadata; None clears the process record. No server is started.
    pub fn update_worktree_server(
        &mut self,
        id: TaskId,
        project: &ProjectId,
        endpoint: &str,
        port: NonZeroU16,
        process: Option<&str>,
    ) -> Result<WorktreeRecord, WorktreeStorageError> {
        if endpoint.is_empty() {
            return Err(WorktreeStorageError::InvalidInput);
        }
        self.mutate_worktree(id,project,|tx,_,now|{tx.execute("UPDATE worktrees SET server_endpoint=?1,server_port=?2,server_process_record=?3,updated_at=?4 WHERE task_id=?5",params![endpoint,port.get(),process,now,id.to_string()])?;event(tx,id,"worktree_server_started",&format!("worktree server: {endpoint}"),now)})
    }
    /// Stores an opaque nonempty baseline. None preserves the existing base head.
    pub fn update_worktree_baseline(
        &mut self,
        id: TaskId,
        project: &ProjectId,
        baseline: &str,
        base_head: Option<&str>,
    ) -> Result<WorktreeRecord, WorktreeStorageError> {
        if baseline.is_empty() {
            return Err(WorktreeStorageError::InvalidInput);
        }
        self.mutate_worktree(id,project,|tx,_,now|{tx.execute("UPDATE worktrees SET baseline_json=?1,base_head=COALESCE(?2,base_head),updated_at=?3 WHERE task_id=?4",params![baseline,base_head,now,id.to_string()])?;Ok(())})
    }
    fn mutate_worktree(
        &mut self,
        id: TaskId,
        project: &ProjectId,
        action: impl FnOnce(&Connection, &WorktreeRecord, &str) -> Result<(), WorktreeStorageError>,
    ) -> Result<WorktreeRecord, WorktreeStorageError> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        owner(&tx, id, project)?;
        let current = wt(&tx, id)?.ok_or(WorktreeStorageError::MissingRecord)?;
        action(&tx, &current, &utc_now_rfc3339_millis())?;
        let result = wt(&tx, id)?.ok_or(WorktreeStorageError::MissingRecord)?;
        tx.commit()?;
        Ok(result)
    }
    pub fn register_worktree_quarantine(
        &mut self,
        id: &WorkflowId,
        original_path: &str,
        input: &WorktreeQuarantineRegistration,
    ) -> Result<WorktreeQuarantineEntry, WorktreeStorageError> {
        if original_path.is_empty() {
            return Err(WorktreeStorageError::InvalidInput);
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = utc_now_rfc3339_millis();
        let found = input
            .found_at
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(&now);
        tx.execute("INSERT INTO worktree_quarantine(entry_id,original_path,quarantined_path,reason,found_at,status) VALUES (?1,?2,?3,?4,?5,?6)",params![id.as_str(),original_path,input.quarantined_path,input.reason,found,input.status.unwrap_or(WorktreeQuarantineStatus::Quarantined).as_str()])?;
        let result = qt(&tx, id)?.ok_or(WorktreeStorageError::MissingRecord)?;
        tx.commit()?;
        Ok(result)
    }
    pub fn get_worktree_quarantine(
        &self,
        id: &WorkflowId,
    ) -> Result<Option<WorktreeQuarantineEntry>, WorktreeStorageError> {
        qt(&self.connection, id)
    }
    pub fn list_worktree_quarantine(
        &self,
    ) -> Result<Vec<WorktreeQuarantineEntry>, WorktreeStorageError> {
        list_quarantine(&self.connection)
    }
    /// Expected values are compared under the same write lock as the transition.
    /// Repeating a status may still update the supplied quarantined path.
    pub fn transition_worktree_quarantine(
        &mut self,
        id: &WorkflowId,
        target: WorktreeQuarantineStatus,
        path: Option<&str>,
        expected_status: Option<WorktreeQuarantineStatus>,
        expected_original_path: Option<&str>,
    ) -> Result<WorktreeQuarantineEntry, WorktreeStorageError> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        registry_schema(&tx)?;
        let current = qt(&tx, id)?.ok_or(WorktreeStorageError::MissingRecord)?;
        if expected_status.is_some_and(|s| s != current.status)
            || expected_original_path.is_some_and(|p| p != current.original_path)
        {
            return Err(WorktreeStorageError::Drift);
        }
        if !current.status.allows(target) {
            return Err(WorktreeStorageError::InvalidTransition);
        }
        tx.execute("UPDATE worktree_quarantine SET status=?1,quarantined_path=COALESCE(?2,quarantined_path) WHERE entry_id=?3",params![target.as_str(),path,id.as_str()])?;
        let result = qt(&tx, id)?.ok_or(WorktreeStorageError::MissingRecord)?;
        tx.commit()?;
        Ok(result)
    }
    pub fn update_worktree_quarantine_status(
        &mut self,
        id: &WorkflowId,
        target: WorktreeQuarantineStatus,
        path: Option<&str>,
    ) -> Result<WorktreeQuarantineEntry, WorktreeStorageError> {
        self.transition_worktree_quarantine(id, target, path, None, None)
    }
}
fn table(c: &Connection, name: &str) -> Result<bool, WorktreeStorageError> {
    Ok(c.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [name],
        |r| r.get(0),
    )?)
}
fn registry_schema(c: &Connection) -> Result<(), WorktreeStorageError> {
    let version: i64 = c.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if !matches!(version, 6 | 11 | 14 | 15) || !table(c, "tasks")? {
        return Err(WorktreeStorageError::IncompatibleSchema);
    }
    let meta: Option<String> = c
        .query_row(
            "SELECT value FROM meta WHERE key='schema_version'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    if meta.as_deref() != Some(version.to_string().as_str()) {
        return Err(WorktreeStorageError::IncompatibleSchema);
    }
    Ok(())
}
fn list_quarantine(c: &Connection) -> Result<Vec<WorktreeQuarantineEntry>, WorktreeStorageError> {
    Ok(c.prepare(&format!(
        "SELECT {QT} FROM worktree_quarantine ORDER BY found_at,entry_id"
    ))?
    .query_map([], quarantine_row)?
    .collect::<rusqlite::Result<Vec<_>>>()?)
}
fn readonly<T>(
    path: &Path,
    missing: T,
    read: impl FnOnce(&Connection) -> Result<T, WorktreeStorageError>,
) -> Result<T, WorktreeStorageError> {
    match std::fs::metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(missing),
        Err(_) => return Err(WorktreeStorageError::IncompatibleSchema),
        Ok(_) => {}
    }
    let mut c =
        open_read_only_current(path).map_err(|_| WorktreeStorageError::IncompatibleSchema)?;
    let tx = c.transaction()?;
    registry_schema(&tx)?;
    let result = read(&tx)?;
    tx.commit()?;
    Ok(result)
}
/// Reads current WAL state through mode=ro; never initializes, migrates or enables WAL.
/// SQLite may touch its transient WAL/SHM sidecars while reading.
pub fn list_worktree_quarantine_readonly(
    path: &Path,
) -> Result<Vec<WorktreeQuarantineEntry>, WorktreeStorageError> {
    readonly(path, Vec::new(), |c| {
        if table(c, "worktree_quarantine")? {
            list_quarantine(c)
        } else {
            Ok(Vec::new())
        }
    })
}
pub fn has_worktree_quarantine_table(path: &Path) -> Result<bool, WorktreeStorageError> {
    readonly(path, false, |c| table(c, "worktree_quarantine"))
}
/// Absence is distinct from unreadable/corrupt state or a missing worktree table.
pub fn read_worktree_readonly_strict(
    path: &Path,
    id: TaskId,
    project: &ProjectId,
) -> Result<Option<WorktreeRecord>, WorktreeStorageError> {
    readonly(path, None, |c| {
        if !table(c, "worktrees")? {
            return Err(WorktreeStorageError::IncompatibleSchema);
        }
        owner(c, id, project)?;
        wt(c, id)
    })
}
