use crate::projects::ProjectService;
use bridge_git::branches::{
    BranchState, branches, operation_in_progress, repository_root, switch_branch,
};
use bridge_git::transfer;
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum GitAction {
    Switch {
        reference: String,
    },
    Create {
        name: String,
        base: String,
        switch: bool,
    },
    Fetch {
        remote: String,
    },
    Preview {
        remote: String,
        destination: String,
    },
    Push {
        remote: String,
        destination: String,
        fingerprint: String,
    },
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitRequest {
    pub workspace: String,
    pub current: Option<String>,
    pub head: Option<String>,
    pub operation: GitAction,
}
fn view(state: BranchState, workspace: &std::path::Path) -> Result<Value, String> {
    let remotes = transfer::remotes(workspace).map_err(|_| "Не удалось прочитать remotes Git")?;
    let upstream =
        transfer::upstream(workspace).map_err(|_| "Не удалось прочитать upstream Git")?;
    let dirty = bridge_git::dirty_paths(workspace)
        .map_err(|_| "Не удалось проверить изменения Git")?
        .len();
    Ok(
        json!({"workspace":workspace,"current":state.current,"head":state.head,"branches":state.branches.iter().map(|b|json!({"reference":b.reference,"name":b.name,"remote":b.remote})).collect::<Vec<_>>(),"remotes":remotes,"upstream":upstream.map(|(remote,destination)|json!({"remote":remote,"destination":destination})),"dirty":dirty}),
    )
}
fn transfer_error(error: bridge_git::GitError) -> String {
    if error == bridge_git::GitError::Timeout {
        "Git не ответил за 30 секунд. Операция остановлена; проверьте состояние remote перед повтором".into()
    } else {
        "Git отклонил операцию. Проверьте доступ к remote, имя ветки и выполните Fetch для обновления удалённой истории".into()
    }
}
impl ProjectService {
    pub fn project_branches(&self, project: &str) -> Result<Value, String> {
        let _activity = self.activity_guard()?;
        let (p, _) = self.project(project)?;
        let state =
            branches(p.workspace()).map_err(|_| "Не удалось прочитать ветки Git проекта")?;
        view(state, p.workspace())
    }
    pub fn switch_project_branch(
        &self,
        project: &str,
        reference: &str,
        expected_current: Option<&str>,
        expected_head: Option<&str>,
        expected_workspace: &str,
    ) -> Result<Value, String> {
        self.project_git(
            project,
            GitRequest {
                workspace: expected_workspace.into(),
                current: expected_current.map(str::to_owned),
                head: expected_head.map(str::to_owned),
                operation: GitAction::Switch {
                    reference: reference.into(),
                },
            },
        )
    }
    pub fn project_git(&self, project: &str, request: GitRequest) -> Result<Value, String> {
        let expected_workspace = request.workspace.as_str();
        let expected_current = request.current.as_deref();
        let expected_head = request.head.as_deref();
        let _activity = self.activity_guard()?;
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
        let reference = match request.operation {
            GitAction::Create { name, base, switch } => {
                if switch
                    && !bridge_git::status_porcelain(&root)
                        .map_err(|_| "Не удалось проверить Git status")?
                        .is_empty()
                {
                    return Err("Есть незакоммиченные или неотслеживаемые файлы. Сохраните изменения перед переключением ветки".into());
                }
                if operation_in_progress(p.workspace())
                    .map_err(|_| "Не удалось проверить состояние Git")?
                {
                    return Err(
                        "Не завершена операция Git. Завершите её перед созданием ветки".into(),
                    );
                }
                transfer::create(p.workspace(),&name,&base,switch).map_err(|_|"Не удалось создать ветку: проверьте имя, исходную ветку и наличие коммитов. Ветка с таким именем может уже существовать")?;
                return view(
                    branches(p.workspace()).map_err(|_| "Не удалось прочитать ветки Git")?,
                    p.workspace(),
                );
            }
            GitAction::Fetch { remote } => {
                transfer::fetch(p.workspace(), &remote).map_err(transfer_error)?;
                return view(
                    branches(p.workspace()).map_err(|_| "Не удалось прочитать ветки Git")?,
                    p.workspace(),
                );
            }
            GitAction::Preview {
                remote,
                destination,
            } => {
                let plan = transfer::preview(p.workspace(), &remote, &destination)
                    .map_err(transfer_error)?;
                return Ok(
                    json!({"current":plan.current,"head":plan.head,"remote":plan.remote,"destination":plan.destination,"remote_head":plan.remote_head,"ahead":plan.ahead,"behind":plan.behind,"new_branch":plan.new_branch,"set_upstream":plan.set_upstream,"fingerprint":plan.fingerprint}),
                );
            }
            GitAction::Push {
                remote,
                destination,
                fingerprint,
            } => {
                if operation_in_progress(p.workspace())
                    .map_err(|_| "Не удалось проверить состояние Git")?
                {
                    return Err("Не завершена операция Git. Завершите её перед push".into());
                }
                let plan = transfer::preview(p.workspace(), &remote, &destination)
                    .map_err(transfer_error)?;
                if plan.fingerprint != fingerprint {
                    return Err("Параметры push изменились. Просмотрите отправку заново".into());
                }
                if plan.behind > 0 {
                    return Err("Удалённая ветка содержит новые коммиты. Выполните Fetch и объедините изменения перед push".into());
                }
                let upstream_saved =
                    transfer::push(p.workspace(), &plan).map_err(transfer_error)?;
                let mut result = view(
                    branches(p.workspace()).map_err(|_| "Не удалось прочитать ветки Git")?,
                    p.workspace(),
                )?;
                result["upstream_saved"] = json!(upstream_saved);
                return Ok(result);
            }
            GitAction::Switch { reference } => reference,
        };
        if !state.branches.iter().any(|b| b.reference == reference) {
            return Err("Выбранная ветка больше недоступна. Обновите список веток".into());
        }
        if state.current.as_deref() == Some(reference.as_str()) {
            return view(state, p.workspace());
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
        switch_branch(p.workspace(),&reference).map_err(|_|"Git отклонил переключение. Ветка может быть занята другим worktree, локальная ветка уже существует или файлы мешают переключению")?;
        let state =
            branches(p.workspace()).map_err(|_| "Не удалось прочитать ветки Git проекта")?;
        view(state, p.workspace())
    }
}
