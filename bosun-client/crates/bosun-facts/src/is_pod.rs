//! Коллектор `is_pod` — детектирует Kubernetes-pod через иерархию проверок.
//!
//! Порядок (первый сработавший побеждает):
//! 1. `BOSUN_FORCE_POD=true|false` (тестовое/ручное переопределение).
//! 2. `<root_fs>/var/run/secrets/kubernetes.io/serviceaccount/token` существует.
//! 3. `KUBERNETES_SERVICE_HOST` непустой.
//! 4. `<root_fs>/proc/1/cgroup` содержит `kubepods` или `containerd`.
//! 5. Иначе `false`.

use std::fs;

use bosun_core::{FactCategory, FactValue, RefreshPolicy};

use crate::collector::{Fact, FactCollectCtx};

pub struct IsPodFact;

impl Fact for IsPodFact {
    fn name(&self) -> &str {
        "is_pod"
    }
    fn category(&self) -> FactCategory {
        FactCategory::Static
    }
    fn refresh_policy(&self) -> RefreshPolicy {
        RefreshPolicy::AtStart
    }
    fn collect(&self, ctx: &FactCollectCtx) -> FactValue {
        // 1. Env override.
        if let Ok(v) = std::env::var("BOSUN_FORCE_POD") {
            match v.as_str() {
                "true" => {
                    tracing::debug!(reason = "env BOSUN_FORCE_POD=true", "is_pod=true");
                    return FactValue::Known(serde_json::Value::Bool(true));
                }
                "false" => {
                    tracing::debug!(reason = "env BOSUN_FORCE_POD=false", "is_pod=false");
                    return FactValue::Known(serde_json::Value::Bool(false));
                }
                _ => {
                    tracing::warn!(
                        value = %v,
                        "BOSUN_FORCE_POD set to non-boolean value, ignoring"
                    );
                }
            }
        }

        // 2. Service-account token.
        let token_path = ctx
            .root_fs
            .join("var/run/secrets/kubernetes.io/serviceaccount/token");
        if token_path.is_file() {
            tracing::debug!(
                reason = "serviceaccount token present",
                path = %token_path.display(),
                "is_pod=true"
            );
            return FactValue::Known(serde_json::Value::Bool(true));
        }

        // 3. KUBERNETES_SERVICE_HOST env var.
        if let Ok(host) = std::env::var("KUBERNETES_SERVICE_HOST") {
            if !host.is_empty() {
                tracing::debug!(
                    reason = "env KUBERNETES_SERVICE_HOST non-empty",
                    "is_pod=true"
                );
                return FactValue::Known(serde_json::Value::Bool(true));
            }
        }

        // 4. /proc/1/cgroup contains kubepods or containerd.
        let cgroup_path = ctx.root_fs.join("proc/1/cgroup");
        if let Ok(content) = fs::read_to_string(&cgroup_path) {
            if content.contains("kubepods") {
                tracing::debug!(reason = "/proc/1/cgroup contains 'kubepods'", "is_pod=true");
                return FactValue::Known(serde_json::Value::Bool(true));
            }
            if content.contains("containerd") {
                tracing::debug!(
                    reason = "/proc/1/cgroup contains 'containerd'",
                    "is_pod=true"
                );
                return FactValue::Known(serde_json::Value::Bool(true));
            }
        }

        // 5. Default false.
        tracing::debug!(reason = "no pod markers detected", "is_pod=false");
        FactValue::Known(serde_json::Value::Bool(false))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::sync::Mutex;

    use tempfile::TempDir;

    use super::*;

    /// `is_pod` читает env-переменные процесса — параллельные тесты будут
    /// гоняться за общим состоянием std::env. Сериализуем доступ через Mutex.
    /// Не используем `lazy_static` — std::sync::OnceLock + замыкание короче.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
        // PoisonError допустим: тест с panic'ом отравит mutex, но семантически
        // последующие тесты могут продолжать — нам нужна только сериализация.
        match LOCK.get_or_init(|| Mutex::new(())).lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }

    fn clear_env() {
        // SAFETY: env-операции safe вне многопоточного use вне теста; env_lock
        // даёт сериализацию.
        std::env::remove_var("BOSUN_FORCE_POD");
        std::env::remove_var("KUBERNETES_SERVICE_HOST");
    }

    #[test]
    fn force_pod_true_returns_known_true() {
        let _g = env_lock();
        clear_env();
        std::env::set_var("BOSUN_FORCE_POD", "true");
        let tmp = TempDir::new().unwrap();
        let ctx = FactCollectCtx::new(tmp.path().to_path_buf());
        let v = IsPodFact.collect(&ctx);
        assert_eq!(v.value().unwrap(), &serde_json::json!(true));
        clear_env();
    }

    #[test]
    fn force_pod_false_returns_known_false() {
        let _g = env_lock();
        clear_env();
        std::env::set_var("BOSUN_FORCE_POD", "false");
        let tmp = TempDir::new().unwrap();
        let ctx = FactCollectCtx::new(tmp.path().to_path_buf());
        let v = IsPodFact.collect(&ctx);
        assert_eq!(v.value().unwrap(), &serde_json::json!(false));
        clear_env();
    }

    #[test]
    fn force_pod_garbage_falls_through() {
        let _g = env_lock();
        clear_env();
        std::env::set_var("BOSUN_FORCE_POD", "maybe");
        let tmp = TempDir::new().unwrap();
        let ctx = FactCollectCtx::new(tmp.path().to_path_buf());
        let v = IsPodFact.collect(&ctx);
        // Нет других маркеров — false.
        assert_eq!(v.value().unwrap(), &serde_json::json!(false));
        clear_env();
    }

    #[test]
    fn serviceaccount_token_detected() {
        let _g = env_lock();
        clear_env();
        let tmp = TempDir::new().unwrap();
        let dir = tmp
            .path()
            .join("var/run/secrets/kubernetes.io/serviceaccount");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("token"), "fake-jwt").unwrap();
        let ctx = FactCollectCtx::new(tmp.path().to_path_buf());
        let v = IsPodFact.collect(&ctx);
        assert_eq!(v.value().unwrap(), &serde_json::json!(true));
    }

    #[test]
    fn kubernetes_service_host_env_detected() {
        let _g = env_lock();
        clear_env();
        std::env::set_var("KUBERNETES_SERVICE_HOST", "10.0.0.1");
        let tmp = TempDir::new().unwrap();
        let ctx = FactCollectCtx::new(tmp.path().to_path_buf());
        let v = IsPodFact.collect(&ctx);
        assert_eq!(v.value().unwrap(), &serde_json::json!(true));
        clear_env();
    }

    #[test]
    fn empty_kubernetes_service_host_falls_through() {
        let _g = env_lock();
        clear_env();
        std::env::set_var("KUBERNETES_SERVICE_HOST", "");
        let tmp = TempDir::new().unwrap();
        let ctx = FactCollectCtx::new(tmp.path().to_path_buf());
        let v = IsPodFact.collect(&ctx);
        assert_eq!(v.value().unwrap(), &serde_json::json!(false));
        clear_env();
    }

    #[test]
    fn cgroup_contains_kubepods() {
        let _g = env_lock();
        clear_env();
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("proc/1");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("cgroup"),
            "12:freezer:/kubepods/burstable/poda123/abc\n",
        )
        .unwrap();
        let ctx = FactCollectCtx::new(tmp.path().to_path_buf());
        let v = IsPodFact.collect(&ctx);
        assert_eq!(v.value().unwrap(), &serde_json::json!(true));
    }

    #[test]
    fn cgroup_contains_containerd() {
        let _g = env_lock();
        clear_env();
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("proc/1");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("cgroup"), "12:freezer:/containerd/abc\n").unwrap();
        let ctx = FactCollectCtx::new(tmp.path().to_path_buf());
        let v = IsPodFact.collect(&ctx);
        assert_eq!(v.value().unwrap(), &serde_json::json!(true));
    }

    #[test]
    fn defaults_to_false_when_no_markers() {
        let _g = env_lock();
        clear_env();
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("proc/1");
        fs::create_dir_all(&dir).unwrap();
        // cgroup без k8s/containerd маркеров.
        fs::write(dir.join("cgroup"), "12:freezer:/\n").unwrap();
        let ctx = FactCollectCtx::new(tmp.path().to_path_buf());
        let v = IsPodFact.collect(&ctx);
        assert_eq!(v.value().unwrap(), &serde_json::json!(false));
    }

    #[test]
    fn missing_everything_returns_known_false() {
        let _g = env_lock();
        clear_env();
        let tmp = TempDir::new().unwrap();
        let ctx = FactCollectCtx::new(tmp.path().to_path_buf());
        let v = IsPodFact.collect(&ctx);
        // Без файлов и env — точно false, не Unknown.
        assert_eq!(v.value().unwrap(), &serde_json::json!(false));
    }
}
