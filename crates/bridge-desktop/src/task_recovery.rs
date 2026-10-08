use crate::projects::ProjectService;
use bridge_worker::recovery::RecoverySpawnOutcome;
use serde_json::{Value, json};
use std::{path::PathBuf, time::Duration};

impl ProjectService {
    pub fn set_task_status(
        &self,
        project: &str,
        task: &str,
        expected: &str,
        target: &str,
        reason: &str,
    ) -> Result<Value, String> {
        let _config = self.config_file_guard()?;
        let (p, layout) = self.project(project)?;
        if let Some(settings) = p.remote_execution() {
            return bridge_automation::remote::rpc(
                settings,
                &json!({"op":"set_status","task":task,"expected":expected,"target":target,"reason":reason}),
            );
        }
        let id = task.parse().map_err(|_| "Некорректный ID задачи")?;
        let expected = expected
            .parse()
            .map_err(|_| "Некорректный текущий статус")?;
        let target = target.parse().map_err(|_| "Некорректный новый статус")?;
        bridge_worker::manual_status::set_status(&layout, &p, id, expected, target, reason)
            .map_err(|code| match code {
                "terminal_task_immutable" => "Принятые и закрытые задачи нельзя изменять",
                "terminal_status_forbidden" => {
                    "Для завершения задачи используйте обычную приёмку или закрытие"
                }
                "task_status_changed" => "Статус уже изменился. Обновите карточку",
                "task_busy" => "Исполнитель занят. Повторите после завершения его работы",
                "invalid_status_reason" => "Укажите причину смены статуса (до 2048 байт)",
                "suspected_secret" => "Причина может содержать секрет. Удалите секрет из текста",
                "task_close_requested" => "Для задачи уже запрошено закрытие",
                _ => "Не удалось изменить статус задачи",
            })?;
        Ok(json!({"status":target}))
    }
    /// Explicitly resume observation of a failed assistant session. Never sends a prompt.
    pub fn recover_failed_task(&self, project: &str, task: &str) -> Result<Value, String> {
        let _config_guard = self.config_file_guard()?;
        let (p, layout) = self.project(project)?;
        if let Some(settings) = p.remote_execution() {
            return bridge_automation::remote::rpc(settings, &json!({"op":"recover","task":task}));
        }

        let id = task.parse().map_err(|_| "Некорректный ID задачи")?;
        bridge_storage::read_task_budget_readonly(&layout.database(), id, p.id())
            .map_err(|_| "Не удалось проверить бюджет задачи")?
            .ok_or("Задача не найдена в этом проекте")?;
        let config = self.config_view()?;
        let layouts = config
            .projects()
            .values()
            .map(|entry| {
                bridge_storage::RustStateLayout::new(self.state.clone(), entry.id().clone())
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| "Некорректный Rust state")?;
        let executable = std::env::var_os("AIBRIDGE_CLI")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/agent-bridge")
            });
        if !executable.is_absolute() || !executable.is_file() {
            return Err("Исполнитель agent-bridge недоступен".into());
        }
        let config_path =
            std::fs::canonicalize(&self.config).map_err(|_| "Конфигурация недоступна")?;
        let result = bridge_worker::recovery::recover_failed_assistant(
            &layout,
            &p,
            id,
            &layouts.iter().collect::<Vec<_>>(),
            Duration::from_secs(2),
            |round| {
                let invocation = bridge_worker::WorkerInvocation::new(
                    &executable,
                    p.id().clone(),
                    &config_path,
                    layout.state_root(),
                    round.task_id,
                    round.round_number,
                )
                .map_err(|_| ())?;
                let worker_layout = layout.clone();
                let workspace = p.workspace().to_owned();
                let (tx, rx) = std::sync::mpsc::sync_channel(1);
                std::thread::Builder::new()
                    .name("desktop-recovery-reaper".into())
                    .spawn(move || {
                        match bridge_worker::spawn_worker(&invocation, &worker_layout, &workspace) {
                            Ok(mut worker) => {
                                let _ = tx.send(Ok(()));
                                let _ = worker.wait();
                            }
                            Err(_) => {
                                let _ = tx.send(Err(()));
                            }
                        }
                    })
                    .map_err(|_| ())?;
                rx.recv().map_err(|_| ())?
            },
        )
        .map_err(|_| "Не удалось проверить привязку задачи, сессии и рабочего каталога")?;
        match result {
            RecoverySpawnOutcome::Spawned(()) => Ok(json!({"status":"observing"})),
            RecoverySpawnOutcome::Busy => Err("Задача занята исполнителем. Повторите проверку позже".into()),
            RecoverySpawnOutcome::Unavailable => Err("Сервер сохранённой сессии недоступен. Запустите сервисы проекта".into()),
            RecoverySpawnOutcome::SpawnFailed => Err("Не удалось запустить проверку. Задача оставлена в исходном состоянии".into()),
            RecoverySpawnOutcome::Unchanged | RecoverySpawnOutcome::Blocked => Err("Состояние задачи изменилось или ошибка не допускает восстановления сессии. Обновите карточку".into()),
        }
    }
}
