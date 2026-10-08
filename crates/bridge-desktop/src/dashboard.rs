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
pub(crate) fn sanitize(value: &Value) -> Value {
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
        let _activity = self.activity_guard()?;
        use std::{
            hash::{Hash, Hasher},
            os::unix::fs::MetadataExt,
        };
        let config = bridge_config::load_config_with_state_root(&self.config, &self.state)
            .map_err(|_| "config invalid")?;
        let selected = config.project(project).ok_or("project not configured")?;
        if let Some(settings) = selected.remote_execution() {
            return bridge_automation::remote::rpc(settings, &json!({"op":"revision"}))
                .map_err(|_| "remote dashboard unavailable")?
                .as_str()
                .map(str::to_owned)
                .ok_or("remote revision invalid");
        }
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
        let _activity = self.activity_guard()?;
        if q.limit == 0 || q.limit > 200 || q.offset > 100000 {
            return Err("invalid page");
        }
        let config = bridge_config::load_config_with_state_root(&self.config, &self.state)
            .map_err(|_| "config invalid")?;
        let selected = config.project(&q.project).ok_or("project not configured")?;
        if let Some(settings) = selected.remote_execution() {
            return bridge_automation::remote::rpc(settings, &json!({"op":"dashboard","query":q}))
                .map_err(|_| "remote dashboard unavailable");
        }
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
            active += storage
                .count_tasks(project.id(), true)
                .map_err(|_| "task data invalid")?;
            let ledger = storage
                .get_active_writers(project.id())
                .map_err(|_| "writer data invalid")?;
            writers += ledger.len();
            reservations.extend(ledger.into_iter().map(|r| json!({"task_id":r.task_id.to_string(),"project_id":r.project_id.as_str(),"parallel":r.parallel,"created_at":r.created_at})));
            waiting += tx.query_row("SELECT COUNT(*) FROM tasks WHERE project_id=?1 AND status='waiting_dependencies'", [project.id().as_str()], |r| r.get::<_, i64>(0)).map_err(|_| "waiting data invalid")?;
            for (_, id, _) in chosen
                .iter()
                .filter(|(id, _, _)| id == project.id().as_str())
            {
                let (title, status, updated_at): (String, String, String) = tx.query_row(
                    "SELECT task,status,updated_at FROM tasks WHERE task_id=?1 AND project_id=?2",
                    params![id, project.id().as_str()],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                ).map_err(|_| "task changed; refresh snapshot")?;
                id.parse::<bridge_domain::TaskId>()
                    .map_err(|_| "task identity invalid")?;
                status
                    .parse::<bridge_domain::TaskStatus>()
                    .map_err(|_| "task data invalid")?;
                let revision = task_revision(&tx, id)?;
                tasks.push(json!({"task_id":id,"project_id":project.id().as_str(),"title":safe_text(&title),"status":status,"updated_at":updated_at,"revision":revision}));
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
    pub fn task_detail(&self, project: &str, task: &str) -> Result<Value, &'static str> {
        let _activity = self.activity_guard()?;
        let (project, layout) = self.project(project)?;
        if let Some(settings) = project.remote_execution() {
            return bridge_automation::remote::rpc(settings, &json!({"op":"detail","task":task}))
                .map_err(|_| "remote task unavailable");
        }
        let storage = layout.open_readonly().map_err(|_| "state unavailable")?;
        let tx = storage
            .connection()
            .unchecked_transaction()
            .map_err(|_| "snapshot unavailable")?;
        let t = storage
            .get_task(task.parse().map_err(|_| "task identity invalid")?)
            .map_err(|_| "task data invalid")?
            .ok_or("task not found")?;
        if t.project_id != *project.id() {
            return Err("task not found");
        }
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
        let recoverable: bool = t.status == bridge_domain::TaskStatus::Failed && t.close_requested_at.is_none() && tx.query_row(
                    "SELECT status='failed' AND error_code='assistant_error' AND attempted=1 AND session_id IS NOT NULL AND outbound_message_id IS NOT NULL FROM rounds WHERE task_id=?1 AND project_id=?2 ORDER BY round_number DESC LIMIT 1",
                    params![t.task_id.to_string(), project.id().as_str()], |r| r.get::<_, Option<bool>>(0),
                ).optional().map_err(|_| "round data invalid")?.flatten().unwrap_or(false);
        let page = round_page(&tx, &t.task_id.to_string(), project.id().as_str(), None)?;
        let revision = task_revision(&tx, &t.task_id.to_string())?;
        let mut all = tx.prepare("SELECT CASE WHEN json_valid(result_json) THEN CAST(json_extract(result_json, '$.usage') AS TEXT) END FROM rounds WHERE task_id=?1 AND project_id=?2 AND result_json IS NOT NULL").map_err(|_| "usage data invalid")?;
        let results = all
            .query_map(params![t.task_id.to_string(), project.id().as_str()], |r| {
                r.get::<_, Option<String>>(0)
            })
            .map_err(|_| "usage data invalid")?;
        let mut usage = bridge_storage::usage::normalize_usage(&Value::Null);
        for result in results {
            let result = result.map_err(|_| "usage data invalid")?;
            usage = bridge_storage::usage::add_usage(
                &usage,
                &bridge_storage::usage::normalize_usage(
                    &result
                        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                        .unwrap_or(Value::Null),
                ),
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
        Ok(
            json!({"task_id":t.task_id.to_string(),"project_id":project.id().as_str(),"title":safe_text(&t.text),"status":t.status,"recoverable":recoverable,"revision_count":t.revision_count,"updated_at":t.updated_at,"execution_mode":mode,"delivery_mode":delivery,"delivery_state":delivery_state,"delivery_refusal":refusal,"workflow_id":workflow,"depends_on":parsed(dependencies),"usage":usage,"budget":budget,"rounds":page["rounds"],"next_before":page["next_before"],"revision":revision,"base_head":t.base_head,"allowed_paths":t.allowed_paths,"test_commands":t.test_commands,"repositories":t.snapshot.as_ref().map(sanitize)}),
        )
    }

    pub fn task_rounds(
        &self,
        project: &str,
        task: &str,
        before: u32,
        expected_revision: &str,
    ) -> Result<Value, &'static str> {
        let _activity = self.activity_guard()?;
        if before == 0 {
            return Err("invalid round cursor");
        }
        let (project, layout) = self.project(project)?;
        if let Some(settings) = project.remote_execution() {
            return bridge_automation::remote::rpc(
                settings,
                &json!({"op":"rounds","task":task,"before":before,"revision":expected_revision}),
            )
            .map_err(|_| "remote task rounds unavailable");
        }
        let storage = layout.open_readonly().map_err(|_| "state unavailable")?;
        let tx = storage
            .connection()
            .unchecked_transaction()
            .map_err(|_| "snapshot unavailable")?;
        let exists: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM tasks WHERE task_id=?1 AND project_id=?2)",
                params![task, project.id().as_str()],
                |r| r.get(0),
            )
            .map_err(|_| "task data invalid")?;
        if !exists {
            return Err("task not found");
        }
        let revision = task_revision(&tx, task)?;
        if revision != expected_revision {
            return Err("История задачи изменилась; обновите карточку");
        }
        round_page(&tx, task, project.id().as_str(), Some(before))
    }
}

// Hash only metadata: refreshing the list never materializes round JSON.
fn task_revision(tx: &rusqlite::Transaction<'_>, task: &str) -> Result<String, &'static str> {
    use std::hash::{Hash, Hasher};
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    let meta: (String, String, u32, Option<String>) = tx.query_row(
        "SELECT updated_at,status,revision_count,close_requested_at FROM tasks WHERE task_id=?1", [task],
        |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)),
    ).map_err(|_| "task data invalid")?;
    meta.hash(&mut hash);
    let mut stmt = tx.prepare("SELECT round_number,status,updated_at FROM rounds WHERE task_id=?1 ORDER BY round_number")
        .map_err(|_| "round metadata invalid")?;
    let rows = stmt
        .query_map([task], |r| {
            Ok((
                r.get::<_, u32>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
        .map_err(|_| "round metadata invalid")?;
    for row in rows {
        row.map_err(|_| "round metadata invalid")?.hash(&mut hash);
    }
    let events: (i64, i64) = tx
        .query_row(
            "SELECT COUNT(*),COALESCE(MAX(id),0) FROM events WHERE task_id=?1",
            [task],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(|_| "event metadata invalid")?;
    events.hash(&mut hash);
    let delivery: Option<Option<String>> = tx
        .query_row(
            "SELECT delivery_state FROM worktrees WHERE task_id=?1",
            [task],
            |r| r.get(0),
        )
        .optional()
        .map_err(|_| "delivery metadata invalid")?;
    delivery.hash(&mut hash);
    Ok(format!("{:016x}", hash.finish()))
}

fn round_page(
    tx: &rusqlite::Transaction<'_>,
    task: &str,
    project: &str,
    before: Option<u32>,
) -> Result<Value, &'static str> {
    let mut stmt = tx.prepare("SELECT round_number,status,structured_findings,checkpoint_json,verifier_json FROM rounds WHERE task_id=?1 AND project_id=?2 AND (?3 IS NULL OR round_number<?3) ORDER BY round_number DESC LIMIT 11")
        .map_err(|_| "round data invalid")?;
    let rows = stmt
        .query_map(params![task, project, before], |r| {
            Ok((
                r.get::<_, u32>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
            ))
        })
        .map_err(|_| "round data invalid")?;
    let mut rounds = Vec::new();
    let mut has_more = false;
    for row in rows {
        if rounds.len() == 10 {
            has_more = true;
            break;
        }
        let (number, status, findings, checkpoint, verification) =
            row.map_err(|_| "round data invalid")?;
        rounds.push(json!({"number":number,"status":status,"findings":parsed(findings),"checkpoint":parsed(checkpoint),"verification":parsed(verification)}));
    }
    let next_before = has_more.then(|| rounds.last().unwrap()["number"].as_u64().unwrap());
    Ok(json!({"rounds":rounds,"next_before":next_before}))
}
