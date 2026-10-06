//! Explicit terminal history pruning. Selection and mutation share one predicate.
use crate::{RustStateLayout, StorageConnection};
use bridge_domain::ProjectId;
use rusqlite::{Connection, params};
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub task_id: String,
    pub status: String,
}
/// Strict ASCII duration, bounded to 100 years.
pub fn duration_seconds(raw: &str) -> Result<u64, &'static str> {
    let (unit, digits) = raw.as_bytes().split_last().ok_or("invalid duration")?;
    if digits.is_empty() || digits.len() > 9 || !digits.iter().all(u8::is_ascii_digit) {
        return Err("invalid duration");
    }
    let n = std::str::from_utf8(digits)
        .map_err(|_| "invalid duration")?
        .parse::<u64>()
        .map_err(|_| "invalid duration")?;
    let scale = match unit {
        b's' => 1,
        b'm' => 60,
        b'h' => 3600,
        b'd' => 86400,
        b'w' => 604800,
        _ => return Err("invalid duration"),
    };
    let seconds = n * scale;
    if seconds == 0 || seconds > 100 * 365 * 86400 {
        return Err("duration outside supported range");
    }
    Ok(seconds)
}
fn select(
    c: &Connection,
    project: &ProjectId,
    cutoff: &str,
    failed: bool,
) -> Result<Vec<Candidate>, &'static str> {
    c.prepare("SELECT task_id,status FROM tasks WHERE project_id=?1 AND (status IN ('accepted','closed') OR (?2 AND status='failed')) AND updated_at < ?3 AND NOT EXISTS(SELECT 1 FROM worktrees w WHERE w.task_id=tasks.task_id AND w.status!='removed') ORDER BY task_id")
        .and_then(|mut q|q.query_map(params![project.as_str(),failed,cutoff],|r|Ok(Candidate{task_id:r.get(0)?,status:r.get(1)?}))?.collect()).map_err(|_|"history selection failed")
}
impl StorageConnection {
    /// Read-only selection, including the live worktree guard.
    pub fn prune_candidates(
        &self,
        project: &ProjectId,
        cutoff: &str,
        failed: bool,
    ) -> Result<Vec<Candidate>, &'static str> {
        select(&self.connection, project, cutoff, failed)
    }
    /// Re-select under BEGIN IMMEDIATE and delete all associated rows atomically.
    pub fn prune_history(
        &mut self,
        project: &ProjectId,
        cutoff: &str,
        failed: bool,
    ) -> Result<Vec<Candidate>, &'static str> {
        let tx = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|_| "history transaction unavailable")?;
        let rows = select(&tx, project, cutoff, failed)?;
        for r in &rows {
            for table in ["events", "rounds", "active_writers", "worktrees", "tasks"] {
                tx.execute(
                    &format!("DELETE FROM {table} WHERE task_id=?1"),
                    [&r.task_id],
                )
                .map_err(|_| "history deletion failed; transaction rolled back")?;
            }
        }
        tx.commit().map_err(|_| "history commit failed")?;
        Ok(rows)
    }
}
/// Dry-run never creates or upgrades state. Maintenance occurs after commit.
pub fn run(
    layout: &RustStateLayout,
    seconds: u64,
    apply: bool,
    failed: bool,
    vacuum: bool,
) -> Result<Vec<Candidate>, &'static str> {
    if vacuum && !apply {
        return Err("--vacuum requires --apply");
    }
    if !layout.database().exists() {
        if layout.marker().exists() {
            return Err("owned state database missing");
        }
        return Ok(vec![]);
    }
    let ro = layout
        .open_readonly()
        .map_err(|_| "history state unowned or incompatible")?;
    let cutoff: String = ro
        .connection
        .query_row(
            "SELECT strftime('%Y-%m-%dT%H:%M:%fZ','now',?1)",
            [format!("-{seconds} seconds")],
            |r| r.get(0),
        )
        .map_err(|_| "history cutoff failed")?;
    if !apply {
        return ro.prune_candidates(layout.project_id(), &cutoff, failed);
    }
    drop(ro);
    let mut storage = layout
        .open()
        .map_err(|_| "history state unowned or incompatible")?;
    let rows = storage.prune_history(layout.project_id(), &cutoff, failed)?;
    let (busy, _, _): (i64, i64, i64) = storage
        .connection
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .map_err(|_| "history deletion committed; checkpoint failed")?;
    if busy != 0 {
        return Err("history deletion committed; checkpoint busy");
    }
    if vacuum {
        storage
            .connection
            .execute_batch("VACUUM")
            .map_err(|_| "history deletion committed; vacuum failed")?;
    }
    Ok(rows)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strict_duration() {
        for s in ["", "0d", "-1h", "1.5d", "١d", "1D", "1d ", "999999999w"] {
            assert!(duration_seconds(s).is_err(), "{s}");
        }
        assert_eq!(duration_seconds("48h"), Ok(172800));
    }
}
