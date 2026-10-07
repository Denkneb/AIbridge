use crate::projects::ProjectService;
use bridge_git::branches::{
    BranchState, branches, operation_in_progress, repository_root, switch_branch,
};
use serde_json::{Value, json};

fn view(state: BranchState, workspace: &std::path::Path) -> Value {
    json!({"workspace":workspace,"current":state.current,"head":state.head,"branches":state.branches.iter().map(|b|json!({"reference":b.reference,"name":b.name,"remote":b.remote})).collect::<Vec<_>>()})
}
impl ProjectService {
    pub fn project_branches(&self, project: &str) -> Result<Value, String> {
        let (p, _) = self.project(project)?;
        let state =
            branches(p.workspace()).map_err(|_| "Не удалось прочитать ветки Git проекта")?;
        Ok(view(state, p.workspace()))
    }
    pub fn switch_project_branch(
        &self,
        project: &str,
        reference: &str,
        expected_current: Option<&str>,
        expected_head: Option<&str>,
        expected_workspace: &str,
    ) -> Result<Value, String> {
        let _lock = self.config_file_guard()?;
        let config = self.config_view()?;
        let p = config.project(project).ok_or("Проект не найден")?;
        if p.workspace().to_str() != Some(expected_workspace) {
            return Err("Workspace проекта изменён. Обновите список веток".into());
        }
        let root = repository_root(p.workspace())
            .map_err(|_| "Workspace проекта не является Git-репозиторием")?;
        // Switching a root also changes any configured nested workspace in it.
        let mut affected = vec![project];
        for other in config.projects().values() {
            if other.id().as_str() != project && other.workspace().starts_with(&root) {
                let other_root = repository_root(other.workspace())
                    .map_err(|_| "Не удалось проверить Git другого проекта в этом workspace")?;
                if other_root == root {
                    affected.push(other.id().as_str());
                }
            }
        }
        let _runtime = bridge_runtime::project::projects_config_edit_guard(
            &config,
            &config,
            &affected,
            &self.state,
        )
        .map_err(|e| e.to_string())?;
        let state =
            branches(p.workspace()).map_err(|_| "Не удалось прочитать ветки Git проекта")?;
        if state.current.as_deref() != expected_current || state.head.as_deref() != expected_head {
            return Err("Ветка или HEAD уже изменились. Обновите список веток".into());
        }
        if !state.branches.iter().any(|b| b.reference == reference) {
            return Err("Выбранная ветка больше недоступна. Обновите список веток".into());
        }
        if state.current.as_deref() == Some(reference) {
            return Ok(view(state, p.workspace()));
        }
        if !bridge_git::status_porcelain(&root)
            .map_err(|_| "Не удалось проверить Git status")?
            .is_empty()
        {
            return Err("Есть незакоммиченные или неотслеживаемые файлы. Сохраните изменения перед переключением ветки".into());
        }
        if operation_in_progress(p.workspace()).map_err(|_| "Не удалось проверить состояние Git")?
        {
            return Err("Не завершена операция Git (merge, rebase или другая). Завершите её перед переключением ветки".into());
        }
        switch_branch(p.workspace(),reference).map_err(|_|"Git отклонил переключение. Ветка может быть занята другим worktree, локальная ветка уже существует или файлы мешают переключению")?;
        self.project_branches(project)
    }
}
