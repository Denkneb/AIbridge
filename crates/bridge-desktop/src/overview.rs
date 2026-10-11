//! Bounded project summaries composed from the existing read-only dashboard.
use crate::{dashboard::Query, projects::ProjectService};
use serde_json::{Value, json};

impl ProjectService {
    /// Never recovers tasks or starts services. Separate status pages ensure
    /// older blockers remain visible even with many newer running tasks.
    pub fn project_overview(&self, project: &str) -> Result<Value, &'static str> {
        let (entry, _) = self.project(project)?;
        let expected = entry
            .remote_execution()
            .map_or(project, |remote| remote.project.as_str());
        let query = |status: Option<&str>| Query {
            project: project.into(),
            active_only: true,
            linked: false,
            offset: 0,
            limit: 12,
            search: String::new(),
            status: status.map(str::to_owned),
        };
        let mut snapshot = self.dashboard(query(None))?;
        if snapshot["errors"]
            .as_array()
            .is_none_or(|errors| !errors.is_empty())
        {
            return Err("project state unavailable");
        }
        let mut attention = vec![];
        let mut attention_count = 0;
        for status in [
            "needs_user",
            "delivery_unknown",
            "failed",
            "awaiting_review",
        ] {
            let page = self.dashboard(query(Some(status)))?;
            if page["errors"]
                .as_array()
                .is_none_or(|errors| !errors.is_empty())
            {
                return Err("project state unavailable");
            }
            attention_count += page["total"].as_u64().ok_or("overview count invalid")?;
            attention.extend(
                page["tasks"]
                    .as_array()
                    .ok_or("overview tasks invalid")?
                    .iter()
                    .cloned(),
            );
        }
        // SSH returns executor project IDs. Navigation must use the registered
        // local alias; reject unexpected identities rather than route to them.
        for task in snapshot["tasks"]
            .as_array_mut()
            .ok_or("overview tasks invalid")?
            .iter_mut()
            .chain(attention.iter_mut())
        {
            if task["project_id"].as_str() != Some(expected) {
                return Err("overview project identity invalid");
            }
            task["project_id"] = json!(project);
        }
        for run in snapshot["runs"]
            .as_array_mut()
            .ok_or("overview runs invalid")?
        {
            run["project_id"] = json!(project);
        }
        Ok(
            json!({"project_id":project,"active_count":snapshot["active_count"],
            "attention_count":attention_count,"tasks":snapshot["tasks"],
            "attention":attention,"runs":snapshot["runs"]}),
        )
    }
}
