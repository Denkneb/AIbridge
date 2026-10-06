//! A bounded read-only snapshot, with no runtime recovery side effects.
use crate::projects::ProjectService;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    pub project: String,
    pub active_only: bool,
    pub linked: bool,
    pub offset: u32,
    pub limit: u32,
    #[serde(default)]
    pub search: String,
    #[serde(default)]
    pub status: Option<String>,
}
fn safe_text(raw: &str) -> String {
    if raw.starts_with('/') {
        return "[private path]".into();
    }
    if bridge_config::suspected_secret_categories(raw).is_empty() {
        raw.chars().take(8192).collect()
    } else {
        "[скрыто: возможные секреты]".into()
    }
}
fn sanitize(value: &Value) -> Value {
    match value {
        Value::String(s) => json!(safe_text(s)),
        Value::Array(a) => Value::Array(a.iter().take(128).map(sanitize).collect()),
        Value::Object(o) => Value::Object(
            o.iter()
                .filter(|(k, _)| {
                    !matches!(
                        k.as_str(),
                        "log"
                            | "logs"
                            | "workspace"
                            | "cwd"
                            | "path"
                            | "checkout"
                            | "runtime_dir"
                            | "password"
                            | "token"
                            | "config_path"
                            | "journal_path"
                            | "delivery_journal_path"
                    )
                })
                .map(|(k, v)| (k.clone(), sanitize(v)))
                .collect(),
        ),
        _ => value.clone(),
    }
}
fn parsed(s: Option<String>) -> Value {
    s.and_then(|s| serde_json::from_str(&s).ok())
        .map(|v| sanitize(&v))
        .unwrap_or(Value::Null)
}
impl ProjectService {
    /// Content-free WAL notification stamp. A periodic full refresh remains the
    /// fallback for filesystem timestamp granularity and database replacement.
    pub fn dashboard_revision(&self, project: &str, linked: bool) -> Result<String, &'static str> {
        use std::{
            hash::{Hash, Hasher},
            os::unix::fs::MetadataExt,
        };
        let config = bridge_config::load_config_with_state_root(&self.config, &self.state)
            .map_err(|_| "config invalid")?;
        let selected = config.project(project).ok_or("project not configured")?;
        let mut projects = vec![selected];
        if linked {
            projects.extend(config.linked_projects(project));
        }
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        std::fs::read(&self.config)
            .map_err(|_| "config unavailable")?
            .hash(&mut hash);
        for project in projects {
            let layout =
                bridge_storage::RustStateLayout::new(self.state.clone(), project.id().clone())
                    .map_err(|_| "state invalid")?;
            project.id().as_str().hash(&mut hash);
            let db = layout.database();
            for path in [
                db.clone(),
                db.with_file_name(format!(
                    "{}-wal",
                    db.file_name().ok_or("state invalid")?.to_string_lossy()
                )),
                layout.marker(),
            ] {
                match std::fs::metadata(path) {
                    Ok(m) => (
                        m.dev(),
                        m.ino(),
                        m.len(),
                        m.mtime(),
                        m.mtime_nsec(),
                        m.ctime(),
                        m.ctime_nsec(),
                    )
                        .hash(&mut hash),
                    Err(_) => 0u8.hash(&mut hash),
                }
            }
        }
        Ok(format!("{:016x}", hash.finish()))
    }

    pub fn dashboard(&self, q: Query) -> Result<Value, &'static str> {
        if q.limit == 0 || q.limit > 200 || q.offset > 100000 {
            return Err("invalid page");
        }
        let config = bridge_config::load_config_with_state_root(&self.config, &self.state)
            .map_err(|_| "config invalid")?;
        let selected = config.project(&q.project).ok_or("project not configured")?;
        let mut projects = vec![selected];
        if q.linked {
            projects.extend(config.linked_projects(&q.project));
        }
        if q.search.len() > 512 {
            return Err("search too long");
        }
        if let Some(status) = &q.status {
            status
                .parse::<bridge_domain::TaskStatus>()
                .map_err(|_| "invalid status filter")?;
        }
        let mut candidates = vec![];
        let mut filtered_total = 0i64;
        let predicate = "project_id=?1 AND (NOT ?2 OR status NOT IN ('accepted','closed')) AND (?3='' OR instr(lower(task),lower(?3))>0 OR instr(task_id,?3)>0) AND (?4 IS NULL OR status=?4)";
        for p in &projects {
            let l = bridge_storage::RustStateLayout::new(self.state.clone(), p.id().clone())
                .map_err(|_| "state invalid")?;
            if !l.database().exists() {
                continue;
            }
            let Ok(storage) = l.open_readonly() else {
                continue;
            };
            let c = storage.connection();
            filtered_total += c
                .query_row(
                    &format!("SELECT COUNT(*) FROM tasks WHERE {predicate}"),
                    params![p.id().as_str(), q.active_only, q.search, q.status],
                    |r| r.get::<_, i64>(0),
                )
                .map_err(|_| "task filter unavailable")?;
            let mut query=c.prepare(&format!("SELECT task_id,updated_at FROM tasks WHERE {predicate} ORDER BY updated_at DESC,task_id LIMIT ?5")).map_err(|_|"task filter unavailable")?;
            for row in query
                .query_map(
                    params![
                        p.id().as_str(),
                        q.active_only,
                        q.search,
                        q.status,
                        i64::from(q.offset) + i64::from(q.limit)
                    ],
                    |r| {
                        Ok((
                            p.id().to_string(),
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                        ))
                    },
                )
                .map_err(|_| "task filter unavailable")?
            {
                candidates.push(row.map_err(|_| "task data invalid")?);
            }
        }
        candidates.sort_by(|a, b| b.2.cmp(&a.2).then(a.1.cmp(&b.1)));
        let chosen = candidates
            .into_iter()
            .skip(q.offset as usize)
            .take(q.limit as usize)
            .collect::<Vec<_>>();
        let mut tasks = vec![];
        let mut active = 0;
        let mut writers = 0;
        let mut waiting = 0i64;
        let mut reservations = vec![];
        let total = filtered_total;
        let mut errors = vec![];
        let mut runs = vec![];
        for project in projects {
            let layout =
                bridge_storage::RustStateLayout::new(self.state.clone(), project.id().clone())
                    .map_err(|_| "state invalid")?;
            if !layout.database().exists() {
                continue;
            }
            let storage = match layout.open_readonly() {
                Ok(s) => s,
                Err(_) => {
                    errors
                        .push(json!({"project":project.id().as_str(),"error":"state unavailable"}));
                    continue;
                }
            };
            let tx = storage
                .connection()
                .unchecked_transaction()
                .map_err(|_| "snapshot unavailable")?;
            let summaries = chosen
                .iter()
                .filter(|(id, _, _)| id == project.id().as_str())
                .map(|(_, id, _)| {
                    storage
                        .get_task(id.parse().map_err(|_| "task identity invalid")?)
                        .map_err(|_| "task data invalid")?
                        .ok_or("task changed; refresh snapshot")
                })
                .collect::<Result<Vec<_>, _>>()?;
            active += storage
                .count_tasks(project.id(), true)
                .map_err(|_| "task data invalid")?;
            let ledger = storage
                .get_active_writers(project.id())
                .map_err(|_| "writer data invalid")?;
            writers += ledger.len();
            reservations.extend(ledger.into_iter().map(|r| json!({"task_id":r.task_id.to_string(),"project_id":r.project_id.as_str(),"parallel":r.parallel,"created_at":r.created_at})));
            waiting += tx.query_row("SELECT COUNT(*) FROM tasks WHERE project_id=?1 AND status='waiting_dependencies'", [project.id().as_str()], |r| r.get::<_, i64>(0)).map_err(|_| "waiting data invalid")?;
            for t in summaries {
                let(workflow,dependencies,mode,delivery,budget):(Option<String>,Option<String>,String,String,Option<String>)=tx.query_row("SELECT workflow_id,depends_on,execution_mode,delivery_mode,budget_json FROM tasks WHERE task_id=?1",[t.task_id.to_string()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).map_err(|_|"task metadata invalid")?;
                let delivery_state: Option<String> = tx
                    .query_row(
                        "SELECT delivery_state FROM worktrees WHERE task_id=?1",
                        [t.task_id.to_string()],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(|_| "delivery metadata invalid")?
                    .flatten();
                let refusal: Option<(String, String)> = tx.query_row("SELECT message,created_at FROM events WHERE task_id=?1 AND kind='delivery_refused' ORDER BY id DESC LIMIT 1", [t.task_id.to_string()], |r| Ok((r.get(0)?,r.get(1)?))).optional().map_err(|_| "delivery refusal invalid")?;
                let refusal = refusal.map(|(message, time)| {
                    let code = message.split(':').next().unwrap_or("");
                    let code = if !code.is_empty()
                        && code.len() <= 64
                        && code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
                    {
                        code
                    } else {
                        "unavailable"
                    };
                    json!({"code":code,"created_at":safe_text(&time)})
                });
                let mut rounds = vec![];

                let mut stmt=tx.prepare("SELECT round_number,status,structured_findings,checkpoint_json,verifier_json,result_json FROM rounds WHERE task_id=?1 AND project_id=?2 ORDER BY round_number DESC LIMIT 100").map_err(|_|"round data invalid")?;
                let rows = stmt
                    .query_map(params![t.task_id.to_string(), project.id().as_str()], |r| {
                        Ok((
                            r.get::<_, u32>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, Option<String>>(2)?,
                            r.get::<_, Option<String>>(3)?,
                            r.get::<_, Option<String>>(4)?,
                            r.get::<_, Option<String>>(5)?,
                        ))
                    })
                    .map_err(|_| "round data invalid")?;
                for row in rows {
                    let (number, status, findings, checkpoint, verification, result) =
                        row.map_err(|_| "round data invalid")?;
                    let _ = result;
                    rounds.push(json!({"number":number,"status":status,"findings":parsed(findings),"checkpoint":parsed(checkpoint),"verification":parsed(verification)}));
                }
                let mut all = tx.prepare("SELECT result_json FROM rounds WHERE task_id=?1 AND project_id=?2 AND result_json IS NOT NULL").map_err(|_| "usage data invalid")?;
                let results = all
                    .query_map(params![t.task_id.to_string(), project.id().as_str()], |r| {
                        r.get::<_, String>(0)
                    })
                    .map_err(|_| "usage data invalid")?;
                let mut usage = bridge_storage::usage::normalize_usage(&Value::Null);
                for result in results {
                    let result = result.map_err(|_| "usage data invalid")?;
                    usage = bridge_storage::usage::add_usage(
                        &usage,
                        &bridge_storage::usage::total_saved_usage([Value::String(result)]),
                    );
                }
                let budget = match budget {
                    None => Value::Null,
                    Some(b) => match bridge_storage::normalize_persisted_budget(Some(&b)) {
                        Ok(Some(b)) => bridge_storage::usage::budget_state(&b, &usage),
                        Ok(None) => Value::Null,
                        Err(_) => json!({"gate":"corrupt"}),
                    },
                };
                tasks.push(json!({"task_id":t.task_id.to_string(),"project_id":project.id().as_str(),"title":safe_text(&t.text),"status":t.status,"revision_count":t.revision_count,"updated_at":t.updated_at,"execution_mode":mode,"delivery_mode":delivery,"delivery_state":delivery_state,"delivery_refusal":refusal,"workflow_id":workflow,"depends_on":parsed(dependencies),"usage":usage,"budget":budget,"rounds":rounds,"base_head":t.base_head,"allowed_paths":t.allowed_paths,"test_commands":t.test_commands,"repositories":t.snapshot.as_ref().map(sanitize)}));
            }
            drop(tx);
            let store = bridge_storage::automation::AutomationRunStore::new(layout);
            match store.load(None) {
                Ok(run) => {
                    let doc = run.document();
                    runs.push(json!({"project_id":project.id().as_str(),"run_id":doc["run_id"],"status":run.status().as_str(),"control":run.control().as_str(),"phase":doc["phase"],"current_step":doc["current_step"],"steps":sanitize(&doc["steps"]),"blocker":sanitize(&doc["blocker"])}));
                }
                Err(bridge_storage::automation::AutomationStoreError::NotFound) => {}
                Err(_) => errors.push(
                    json!({"project":project.id().as_str(),"error":"automation state unavailable"}),
                ),
            }
        }
        tasks.sort_by(|a, b| {
            b["updated_at"]
                .as_str()
                .cmp(&a["updated_at"].as_str())
                .then(a["task_id"].as_str().cmp(&b["task_id"].as_str()))
        });
        Ok(
            json!({"tasks":tasks,"total":total,"active_count":active,"writer_count":writers,"waiting_count":waiting,"reservations":reservations,"runs":runs,"errors":errors,"offset":q.offset,"limit":q.limit}),
        )
    }
}
