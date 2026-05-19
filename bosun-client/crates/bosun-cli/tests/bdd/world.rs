//! Состояние BDD-сценария.
//!
//! Один `BosunWorld` живёт от Before-хука до After-хука сценария.
//! Все ресурсы (контейнер, tempdir с bundle'ом) очищаются в After-хуке
//! или через Drop.

use std::path::PathBuf;

use cucumber::World;
use tempfile::TempDir;

/// Результат выполнения команды внутри контейнера (через `docker exec`).
#[derive(Debug, Clone, Default)]
pub struct DockerExecResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl DockerExecResult {
    pub fn combined(&self) -> String {
        let mut s = String::with_capacity(self.stdout.len() + self.stderr.len() + 1);
        s.push_str(&self.stdout);
        if !self.stderr.is_empty() {
            if !s.ends_with('\n') && !s.is_empty() {
                s.push('\n');
            }
            s.push_str(&self.stderr);
        }
        s
    }
}

/// Состояние сценария.
#[derive(Default, World)]
#[world(init = Self::default)]
pub struct BosunWorld {
    /// ID запущенного docker-контейнера.
    pub container_id: Option<String>,
    /// Workdir внутри контейнера, в котором лежит bundle.
    pub container_workdir: String,
    /// Tempdir на хосте, куда складываются файлы bundle до `docker cp`.
    pub bundle_tmp: Option<TempDir>,
    /// Тело manifest'а (main.star в корне).
    pub manifest_body: Option<String>,
    /// Тело inventory (yaml).
    pub inventory_yaml: Option<String>,
    /// Тело bundle.toml — поверх дефолта.
    pub bundle_toml: Option<String>,
    /// Шаблоны для templates/, ключ — относительный путь под templates/.
    pub templates: Vec<(String, String)>,
    /// Результат последнего `docker exec` (для шагов «Then exit code is N»
    /// и «Then stdout contains «<text>»»).
    pub last_exec: Option<DockerExecResult>,
    /// AbortHandle slow-warning задачи для отмены в After-hook.
    pub slow_warning_abort: Option<tokio::task::AbortHandle>,
    /// Путь к bosun-бинарю на хосте, который копируется в контейнер.
    pub bosun_binary_path: PathBuf,
    /// Override для факта `init_system`. Если задан — попадает в
    /// `bosun apply --init-system <value>`. Используется сценариями под
    /// runr/systemd, у которых PID 1 контейнера не совпадает с реальным
    /// init-системой (`tail -f /dev/null` классифицируется как `unknown`,
    /// но сценарий поднимает supervisor поверх и хочет ветку runr/systemd).
    pub init_system_override: Option<String>,
    /// Снимки PID'ов runr-сервисов, сохранённые шагом
    /// `Given I remember pid of runr service "<name>" as "<label>"`.
    /// Используется в assertion'ах вида «PID после restart отличается от
    /// PID до restart», поскольку реальный runr не инкрементит счётчик
    /// `restarts` на манульные API-вызовы (см. runr_helper).
    pub runr_pid_snapshots: std::collections::HashMap<String, u32>,
}

impl std::fmt::Debug for BosunWorld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BosunWorld")
            .field("container_id", &self.container_id)
            .field("container_workdir", &self.container_workdir)
            .field("bundle_tmp", &self.bundle_tmp.as_ref().map(|d| d.path()))
            .field(
                "last_exec_exit",
                &self.last_exec.as_ref().map(|r| r.exit_code),
            )
            .finish()
    }
}
