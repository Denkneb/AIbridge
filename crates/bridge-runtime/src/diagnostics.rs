//! Fixed-shape read-only diagnostics. SQL/task/model text never reaches output.
use bridge_config::ProjectEntry;
use bridge_domain::{TaskId, TaskStatus};
use bridge_storage::RustStateLayout;
use rusqlite::{Connection, OptionalExtension};
use serde_json::{Value, json};
use std::{collections::BTreeMap, time::Duration};
#[derive(Clone)]
pub struct Brief {
    pub project_id: String,
    pub task_id: TaskId,
    pub status: TaskStatus,
    pub updated_at: String,
    pub phase: Option<&'static str>,
    pub round: Option<u32>,
    pub progress: Option<Value>,
}
pub fn briefs(layout: &RustStateLayout) -> Result<Vec<Brief>, &'static str> {
    let s = layout.open_readonly().map_err(|_| "state_unavailable")?;
    let tx = s
        .connection()
        .unchecked_transaction()
        .map_err(|_| "state_unavailable")?;
    read_briefs(&tx, layout)
}
fn read_briefs(c: &Connection, layout: &RustStateLayout) -> Result<Vec<Brief>, &'static str> {
    let mut q=c.prepare("SELECT task_id,status,updated_at FROM tasks WHERE project_id=?1 AND status NOT IN ('accepted','closed') ORDER BY CASE WHEN status='waiting_dependencies' THEN 1 ELSE 0 END,created_at,task_id").map_err(|_|"state_unavailable")?;
    let rows = q
        .query_map([layout.project_id().as_str()], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
        .map_err(|_| "state_unavailable")?;
    let mut out = vec![];
    for row in rows {
        let (id, status, time) = row.map_err(|_| "state_unavailable")?;
        let task_id: TaskId = id.parse().map_err(|_| "state_corrupt")?;
        let status: TaskStatus = status.parse().map_err(|_| "state_corrupt")?;
        if !status.is_active() || !safe_timestamp(&time) || task_id.to_string() != id {
            return Err("state_corrupt");
        }
        let round=c.query_row("SELECT round_number,verifier_state,verifier_json FROM rounds WHERE task_id=?1 AND project_id=?2 ORDER BY round_number DESC LIMIT 1",rusqlite::params![id,layout.project_id().as_str()],|r|Ok((r.get::<_,u32>(0)?,r.get::<_,Option<String>>(1)?,r.get::<_,Option<String>>(2)?))).optional().map_err(|_|"state_corrupt")?;
        let inflight = matches!(status, TaskStatus::Implementing | TaskStatus::Revising);
        let verifying = inflight
            && round
                .as_ref()
                .is_some_and(|r| r.1.as_deref() == Some("running"));
        let progress = if verifying {
            let v = round
                .as_ref()
                .and_then(|r| r.2.as_ref())
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                .unwrap_or(Value::Null);
            let count = v["command_count"].as_u64().unwrap_or(0);
            Some(
                json!({"state":"running","command_index":v["command_index"].as_u64().unwrap_or(0).min(count),"command_count":count}),
            )
        } else {
            None
        };
        out.push(Brief {
            project_id: layout.project_id().to_string(),
            task_id,
            status,
            updated_at: time,
            phase: if inflight {
                Some(if verifying { "verifying" } else { "agent" })
            } else {
                None
            },
            round: round.map(|r| r.0),
            progress,
        });
    }
    Ok(out)
}
pub fn safe_timestamp(s: &str) -> bool {
    if s.len() <= 32
        && let Some(utc) = s.strip_suffix("+00:00")
    {
        return safe_timestamp(&format!("{utc}Z"));
    }
    let b = s.as_bytes();
    if !(20..=27).contains(&b.len())
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
        || b[b.len() - 1] != b'Z'
    {
        return false;
    }
    if b.len() > 20 && (b[19] != b'.' || !b[20..b.len() - 1].iter().all(u8::is_ascii_digit)) {
        return false;
    }
    for (a, z) in [(0, 4), (5, 7), (8, 10), (11, 13), (14, 16), (17, 19)] {
        if !b[a..z].iter().all(u8::is_ascii_digit) {
            return false;
        }
    }
    let n = |a, z| s[a..z].parse::<u32>().unwrap();
    let y = n(0, 4);
    let m = n(5, 7);
    let d = n(8, 10);
    let days = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
                29
            } else {
                28
            }
        }
        _ => 0,
    };
    y > 0 && d > 0 && d <= days && n(11, 13) < 24 && n(14, 16) < 60 && n(17, 19) < 60
}
pub fn snapshot(layout: &RustStateLayout) -> Result<Value, &'static str> {
    let s = layout.open_readonly().map_err(|_| "state_unavailable")?;
    let tx = s
        .connection()
        .unchecked_transaction()
        .map_err(|_| "state_unavailable")?;
    let active = read_briefs(&tx, layout)?;
    let mut counts = BTreeMap::new();
    let mut q = tx
        .prepare("SELECT status,COUNT(*) FROM tasks WHERE project_id=?1 GROUP BY status")
        .map_err(|_| "state_unavailable")?;
    let rows = q
        .query_map([layout.project_id().as_str()], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })
        .map_err(|_| "state_unavailable")?;
    for r in rows {
        let (k, n) = r.map_err(|_| "state_unavailable")?;
        let status: TaskStatus = k.parse().map_err(|_| "state_corrupt")?;
        counts.insert(status.as_str(), n);
    }
    let writers: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM active_writers WHERE project_id=?1",
            [layout.project_id().as_str()],
            |r| r.get(0),
        )
        .map_err(|_| "state_unavailable")?;
    Ok(
        json!({"project_id":layout.project_id(),"tasks_by_status":counts,"active_tasks":active.iter().map(|b|json!({"task_id":b.task_id,"status":b.status.as_str(),"updated_at":b.updated_at,"round_number":b.round,"phase":b.phase,"verification_progress":b.progress})).collect::<Vec<_>>(),"active_writer_count":writers,"waiting_count":active.iter().filter(|b|b.status==TaskStatus::WaitingDependencies).count(),"db_size_bytes":std::fs::metadata(layout.database()).map(|m|m.len()).unwrap_or(0)}),
    )
}
pub fn status(project: &ProjectEntry, layout: &RustStateLayout, timeout: Duration) -> Value {
    if crate::project::validate(project, layout).is_err() {
        return json!({"project_id":project.id(),"ready":false,"servers":{"opencode":{"ready":false,"managed":false,"process_record":"invalid"},"mcp":{"ready":false,"managed":false,"process_record":"invalid"}},"snapshot":{"project_id":project.id(),"error":"state_unavailable"}});
    }
    let no_tasks = layout
        .open_readonly()
        .ok()
        .and_then(|s| s.count_tasks(project.id(), true).ok())
        == Some(0);
    let mut servers = json!({});
    let mut all = true;
    for kind in ["opencode", "mcp"] {
        let record = crate::readiness::record_state(layout, project, kind).unwrap_or("invalid");
        let ready = record != "invalid"
            && if kind == "opencode" {
                crate::readiness::opencode(project, timeout)
            } else {
                crate::readiness::mcp(project, timeout)
            };
        let idle = kind == "opencode"
            && (no_tasks || project.execution_mode() == bridge_domain::ExecutionMode::Worktree)
            && !ready
            && matches!(record, "missing" | "stale");
        all &= ready || idle;
        servers[kind] =
            json!({"ready":ready,"idle":idle,"managed":record=="live","process_record":record});
    }
    json!({"project_id":project.id(),"ready":all,"servers":servers,"snapshot":snapshot(layout).unwrap_or_else(|e|json!({"project_id":project.id(),"error":e}))})
}
