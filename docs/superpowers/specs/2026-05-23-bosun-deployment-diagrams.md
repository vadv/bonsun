# bosun-server (внутри chiit-server) — deployment и lifecycle диаграммы

Date: 2026-05-23
Companion to: [bosun-api-p0-proto.md](./2026-05-23-bosun-api-p0-proto.md)

ASCII-диаграммы по решениям 2026-05-22 / 2026-05-23. Это рабочая картинка, чтобы видеть кто куда подключается и какие пути данных.

---

## 1. Deployment topology

```
                                ┌─────────────────────────────────────────┐
                                │           Operator / CI                 │
                                │  • bosun apply --bundle=PATH (local)    │
                                │  • bosun issue-rollout (через CLI)      │
                                │  • Web UI — ПОКА НЕТ                    │
                                └──────────────────┬──────────────────────┘
                                                   │  admin-token / Keycloak JWT
                                                   │  IssueCommand / IssueRollout / Get*
                                                   ▼
                                ┌─────────────────────────────────────────┐
                                │  Kubernetes Service: chiit-server       │
                                │  (DNS: chiit-server-headless.infra.svc) │
                                └────┬─────────────────┬─────────────────┬┘
                                     │                 │                 │
                                     ▼                 ▼                 ▼
                              ┌──────────┐      ┌──────────┐      ┌──────────┐
                              │ pod_0    │      │ pod_1    │      │ pod_2    │
                              │ chiit-   │      │ chiit-   │      │ chiit-   │
                              │ server   │      │ server   │      │ server   │
                              │          │      │          │      │          │
                              │ ChiitAPI │      │ ChiitAPI │      │ ChiitAPI │
                              │ BosunAPI │      │ BosunAPI │      │ BosunAPI │
                              └────┬─────┘      └────┬─────┘      └────┬─────┘
                                   │                 │                 │
                                   └─────────┬───────┴─────────┬───────┘
                                             │                 │
                                             ▼                 ▼
                                  ┌──────────────────┐  ┌──────────────────┐
                                  │   PostgreSQL     │  │   Vault          │
                                  │  (chiit DB)      │  │   (secrets)      │
                                  │                  │  │                  │
                                  │ bosun_pods       │  │ infra/postgresql │
                                  │ bosun_heartbeats │  │   /<db>/<env>:*  │
                                  │ bosun_active_*   │  │                  │
                                  │ bosun_commands_* │  │ chiit/bootstrap- │
                                  │ bosun_rollouts   │  │   token          │
                                  │ bosun_bundles    │  │                  │
                                  │ chiit_validators │  └──────────────────┘
                                  │ chiit_audit_log  │
                                  └────────┬─────────┘
                                           │
                                           │ LISTEN bosun_command_<host>
                                           │ NOTIFY (cross-replica)
                                           │
                                ╔══════════╧══════════════════════════════╗
                                ║   60k nodes (KVM + k8s pods)            ║
                                ║                                         ║
                                ║  ┌──────────────┐    ┌──────────────┐  ║
                                ║  │ node_001     │    │ node_002     │  ║
                                ║  │  ┌─────────┐ │    │  ┌─────────┐ │  ║
                                ║  │  │ bosun-  │ │    │  │ bosun-  │ │  ║
                                ║  │  │ client  │ │    │  │ client  │ │  ║
                                ║  │  │ (Rust)  │ │    │  │ (Rust)  │ │  ║
                                ║  │  └────┬────┘ │    │  └────┬────┘ │  ║
                                ║  │       │     │    │       │     │  ║
                                ║  │  ┌────┴────┐ │    │       │     │  ║
                                ║  │  │ chiit-  │ │    │  (only bosun) │  ║
                                ║  │  │ client  │ │    │              │  ║
                                ║  │  │ (Go,    │ │    │              │  ║
                                ║  │  │ legacy) │ │    │              │  ║
                                ║  │  └─────────┘ │    │              │  ║
                                ║  └──────────────┘    └──────────────┘  ║
                                ║                                         ║
                                ║  Transition period: chiit-client и     ║
                                ║  bosun-client могут жить на одной ноде ║
                                ║  параллельно. Один ECDSA-key через     ║
                                ║  общую таблицу chiit_validators.       ║
                                ╚═════════════════════════════════════════╝
```

---

## 2. Lifecycle: bosun-client при первом старте

```
                       node booted, no /etc/bosun/client.pem
                                       │
                                       ▼
                       ┌───────────────────────────────────┐
                       │ bosun-client читает               │
                       │ /etc/bosun/registration_token     │
                       │ (доставлен через cloud-init /     │
                       │  image / chef baseline)           │
                       └───────────────┬───────────────────┘
                                       │
                                       ▼
                       ┌───────────────────────────────────┐
                       │ Сгенерировать ECDSA P-256 keypair │
                       │ (rcgen + ring уже в Rust workspace) │
                       └───────────────┬───────────────────┘
                                       │
                                       ▼
              chiit-server-headless.infra.svc:8443 → k8s round-robin → pod_X
                                       │
                                       ▼
                       ┌───────────────────────────────────┐
                       │ Bootstrap(host, token, pubkey)    │
                       └───────────────┬───────────────────┘
                                       │
                                       ▼
                       ┌───────────────────────────────────┐
                       │ pod_X:                            │
                       │ 1. Проверить registration_token   │
                       │ 2. INSERT/UPDATE chiit_validators │
                       │    (общая таблица с chiit-client) │
                       │ 3. Подписать cert chiit CA        │
                       │ 4. Записать audit_log             │
                       └───────────────┬───────────────────┘
                                       │
                                       ▼
                       ┌───────────────────────────────────┐
                       │ bosun-client:                     │
                       │ Записать cert_pem в               │
                       │ /etc/bosun/client.pem (0600)      │
                       │ Затереть registration_token       │
                       └───────────────┬───────────────────┘
                                       │
                                       ▼
                                  бутстрап OK
                                       │
                                       ▼
                            переход к Subscribe (см. диаграмму 3)
```

---

## 3. Lifecycle: Subscribe с pod redirect

```
                       bosun-client (есть cert)
                                       │
                                       ▼  k8s round-robin → pod_0
                       ┌───────────────────────────────────┐
                       │ Subscribe(host, sharding_key=     │
                       │   "ledger-cluster", capabilities) │
                       │ ECDSA signature в metadata        │
                       └───────────────┬───────────────────┘
                                       │
                                       ▼
                       ┌───────────────────────────────────┐
                       │ pod_0: scratch stream interceptor │
                       │ → ECDSA validator → ok            │
                       │ → target = crc32("ledger-cluster")│
                       │     % active_pods_count           │
                       │ → suppose target=2, self=0        │
                       └───────────────┬───────────────────┘
                                       │
                                       ▼
                       ┌───────────────────────────────────┐
                       │ pod_0 → client:                   │
                       │ Command{Redirect{                 │
                       │   target_pod_addr=                │
                       │     "chiit-server-2.headless:8443"│
                       │   reason="hash routes to pod_2",  │
                       │   grace=5s                        │
                       │ }}                                │
                       │ Close stream gracefully.          │
                       └───────────────┬───────────────────┘
                                       │
                                       ▼
                       ┌───────────────────────────────────┐
                       │ bosun-client:                     │
                       │ disconnect от pod_0               │
                       │ dial target_pod_addr (pod_2)      │
                       │ Subscribe() заново                │
                       └───────────────┬───────────────────┘
                                       │
                                       ▼
                       ┌───────────────────────────────────┐
                       │ pod_2: target=self → ok           │
                       │ → INSERT bosun_active_sessions    │
                       │   (host, pod_id=pod_2, ...)       │
                       │ → LISTEN bosun_command_<host>     │
                       │ → start sending pending commands  │
                       └───────────────┬───────────────────┘
                                       │
                                       ▼
                       ┌───────────────────────────────────┐
                       │ Goroutine на pod_2:               │
                       │ • каждые 30s ожидает Heartbeat    │
                       │ • реагирует на NOTIFY от PG       │
                       │ • стримит Command по ходу         │
                       └───────────────────────────────────┘
```

---

## 4. Lifecycle: Rollout с failure-rate halt

```
              operator CLI: bosun issue-rollout
              --bundle=42 --target severity:medium
              --over 30m --max-failure-rate 0.05
                              │
                              ▼  admin-token / Keycloak JWT
                ┌──────────────────────────────────┐
                │ IssueRollout(target, payload,    │
                │   over_duration=30m,             │
                │   max_failure_rate=0.05,         │
                │   failure_mode=STRICT,           │
                │   max_in_flight=200)             │
                └──────────────┬───────────────────┘
                              │
                              ▼
                ┌──────────────────────────────────┐
                │ pod_handle:                      │
                │ 1. Expand target →               │
                │    1500 hosts (severity=medium)  │
                │ 2. INSERT bosun_rollouts         │
                │    state=pending → running       │
                │ 3. Return rollout_id             │
                └──────────────┬───────────────────┘
                              │
                              ▼
                ┌──────────────────────────────────┐
                │ Background dispatcher worker     │
                │ (отдельный goroutine, leader-    │
                │  elected через advisory_lock)    │
                │ цикл каждые 5s:                  │
                └──────────────┬───────────────────┘
                              │
        ┌─────────────────────┴─────────────────────┐
        │                                            │
        ▼                                            ▼
┌─────────────────────┐                  ┌─────────────────────┐
│ Расчёт прогресса:   │                  │ Проверка failure-   │
│ • elapsed / over_dur│                  │ rate:               │
│ • expected_dispatch │                  │ failed/(succeeded+  │
│   = total * ratio   │                  │  failed) > 0.05 ?   │
│ • actual_dispatched │                  │                     │
│ • diff = новые      │                  │ YES → state=halted  │
│   targets для INSERT│                  │     halt_reason=    │
└──────────┬──────────┘                  │     "5.2% failed"   │
           │                              │ NO → продолжаем     │
           ▼                              └─────────────────────┘
┌──────────────────────────────────┐
│ INSERT bosun_commands_queue      │
│ (batch для N target hosts,       │
│  N = expected - actual)          │
│ → per-host NOTIFY                │
│   bosun_command_<host>           │
└──────────────┬───────────────────┘
               │
               ▼
┌──────────────────────────────────┐
│ Pod, который держит Subscribe    │
│ для каждого host'а — получает    │
│ NOTIFY, SELECT pending, Send в   │
│ stream → bosun-client применяет  │
└──────────────┬───────────────────┘
               │
               ▼
┌──────────────────────────────────┐
│ bosun-client → ReportApplyResult │
│ (exit_code, resources_*, errors) │
└──────────────┬───────────────────┘
               │
               ▼
┌──────────────────────────────────┐
│ pod_handle:                      │
│ 1. UPDATE bosun_commands_queue   │
│    SET acked_at=NOW()            │
│ 2. UPDATE bosun_rollouts         │
│    SET acked_targets++,          │
│        succeeded/failed++        │
│ → следующий цикл worker'а        │
│   пересчитает failure_rate       │
└──────────────────────────────────┘
```

Состояния:

```
   ┌──────────┐  IssueRollout                          ┌──────────────────┐
   │ pending  │ ─────────────────────────────────────► │ running          │
   └──────────┘                                        └─────┬────────────┘
                                                              │
                            PauseRollout                       │
       ┌──────────┐ ◄──────────────────────────────────────────┤
       │ paused   │                                            │
       └────┬─────┘                                            │
            │  ResumeRollout                                   │
            └───────────────────────────────────────────────►──┤
                                                                │
                                                  failure_rate>max
       ┌──────────┐                                              │
       │ halted   │ ◄────────────────────────────────────────────┤
       └────┬─────┘                                              │
            │  ResumeRollout (operator решает что фейлы ок)      │
            └───────────────────────────────────────────────►────┤
                                                                  │
                                                          AbortRollout
       ┌──────────┐ ◄──────────────────────────────────────────────┤
       │ aborted  │                                                │
       └──────────┘                                                │
                                                                   │
                                       все таргеты acked'нуты      │
       ┌──────────┐ ◄────────────────────────────────────────────►─┘
       │ completed│
       └──────────┘
```

---

## 5. Lifecycle: pod shutdown drain (SIGTERM)

```
                    k8s sends SIGTERM (rolling update / HPA scale-down)
                                       │
                                       ▼
                       ┌───────────────────────────────────┐
                       │ pod_2: signal handler             │
                       └───────────────┬───────────────────┘
                                       │
                                       ▼
                       ┌───────────────────────────────────┐
                       │ 1. UPDATE bosun_pods              │
                       │    SET draining=TRUE              │
                       │    WHERE pod_id=$self             │
                       │ → NOTIFY bosun_pods_changed       │
                       └───────────────┬───────────────────┘
                                       │
                                       ▼
                       ┌───────────────────────────────────┐
                       │ Все другие pods получают NOTIFY → │
                       │ refresh active_pods list (без     │
                       │ pod_2). Новые Subscribe'ы НЕ      │
                       │ роутятся на pod_2.                │
                       └───────────────────────────────────┘

                       Тем временем на pod_2:
                                       │
                                       ▼
                       ┌───────────────────────────────────┐
                       │ Пройти по всем live sessions:     │
                       │ для каждого session:              │
                       │   target = pods.Target(           │
                       │     session.sharding_key)         │
                       │     // без draining pods          │
                       │   Send(Command{Redirect{          │
                       │     target_pod_addr=target.addr,  │
                       │     reason="pod_2 shutting down", │
                       │     grace=30s                     │
                       │   }})                             │
                       └───────────────┬───────────────────┘
                                       │
                                       ▼
                       ┌───────────────────────────────────┐
                       │ Ждать drain_timeout=30s.          │
                       │ Клиенты переподключаются.         │
                       │ session.Close() по ходу.          │
                       └───────────────┬───────────────────┘
                                       │
                                       ▼
                       ┌───────────────────────────────────┐
                       │ 30s истекли → force-close любых   │
                       │ оставшихся streams.               │
                       │ DELETE FROM bosun_pods            │
                       │   WHERE pod_id=$self.             │
                       │ os.Exit(0)                        │
                       └───────────────────────────────────┘
```

---

## 6. Lifecycle: bundle publish + distribute

Bundle immutable — записи в `bosun_bundles` только INSERT'ятся, никогда не UPDATE. Каждая публикация создаёт новую `version BIGSERIAL`. CLI `bosun bundle publish` для оператора **не существует** — bundle приходит в chiit-server через автоматизацию (CI/CD pipeline, отдельный orchestrator). Здесь диаграмма показывает уже эту автоматизацию.

```
       CI/CD pipeline: build bundle → sign → push в chiit-server
                              │
                              ▼  admin-token (CI service-account)
              ┌──────────────────────────────────┐
              │ chiit-server RPC: PublishBundle  │
              │ (вызывается automation'ом, не    │
              │  вручную)                        │
              │ ① compute sha256                 │
              │ ② verify signature (ed25519)     │
              │ ③ INSERT bosun_bundles           │
              │   (version BIGSERIAL, blob,      │
              │    sha256, signature, tags)      │
              │   — ТОЛЬКО INSERT, никаких UPDATE│
              │ ④ return version                 │
              └──────────────┬───────────────────┘
                              │
                              ▼  rollout уже создан / создаётся следом
              ┌──────────────────────────────────┐
              │ IssueRollout(version=42, target) │
              │ → INSERT bosun_commands_queue    │
              │   (per-host записи)              │
              │ → NOTIFY каждому соответствующему│
              │   pod'у                          │
              └──────────────┬───────────────────┘
                              │
                              ▼
              ┌──────────────────────────────────┐
              │ Pod держащий host'а → Send       │
              │ Command{ApplyBundle{             │
              │   bundle_version=42,             │
              │   tags=["production"]            │
              │ }}                               │
              └──────────────┬───────────────────┘
                              │
                              ▼
              ┌──────────────────────────────────┐
              │ bosun-client получил команду.    │
              │ → GetBundleManifest(version=42)  │
              │ → если sha256 не совпадает с     │
              │   локальным кешем, GetBundleBlob │
              │   (streaming chunks из PG bytea) │
              │ → verify signature               │
              │ → distill bundle на диск         │
              │ → bosun apply --bundle=...       │
              │ → ReportApplyResult              │
              └──────────────────────────────────┘
```

---

## 7. Bootstrap-flow: бесшовный переход chiit → bosun

```
       Состояние «до»:
       ┌──────────────────────────────┐    ┌─────────────────────────┐
       │ chiit_validators             │    │ Node                    │
       │ ┌─────────────────────────┐  │    │ ┌─────────────────────┐ │
       │ │ host=N1                 │  │    │ │ chiit-client active │ │
       │ │ pubkey=K_chiit          │  │ ◄──┤ │ /etc/chiit/         │ │
       │ │ cert_serial=42          │  │    │ │   client.pem        │ │
       │ │ not_after=2026-12-01    │  │    │ │   (private K_chiit) │ │
       │ │ source=chiit            │  │    │ └─────────────────────┘ │
       │ └─────────────────────────┘  │    └─────────────────────────┘
       └──────────────────────────────┘

       Шаг 1: на ноду доставлен bosun-binary + registration_token
       ┌─────────────────────────────────────────────────────────┐
       │ Node                                                    │
       │ ┌─────────────────┐  ┌─────────────────┐                │
       │ │ chiit-client    │  │ bosun-client    │                │
       │ │ (всё ещё там)   │  │ (только что     │                │
       │ │                 │  │  старт)         │                │
       │ │ /etc/chiit/     │  │ /etc/bosun/     │                │
       │ │   client.pem    │  │   registration_ │                │
       │ │   (K_chiit)     │  │   token         │                │
       │ └─────────────────┘  └────────┬────────┘                │
       └─────────────────────────────────┼─────────────────────────┘
                                         │
                                         ▼
                              Bootstrap(host=N1, token, pubkey=K_bosun_new)

       Шаг 2: chiit-server обновляет общую запись
       ┌──────────────────────────────┐
       │ chiit_validators             │
       │ ┌─────────────────────────┐  │
       │ │ host=N1                 │  │
       │ │ pubkey=K_bosun_new      │  │  ← rotation
       │ │ cert_serial=43          │  │  ← новый cert
       │ │ not_after=2027-05-23    │  │
       │ │ source=bosun            │  │  ← обновлён marker
       │ │ rotated_at=2026-05-23   │  │  ← audit
       │ └─────────────────────────┘  │
       └──────────────────────────────┘
                       │
                       │ ChiitAPI агентам с K_chiit теперь падает (key mismatch).
                       │ Старый chiit-client получает ECDSA-error на следующем
                       │ запросе → паника / журнал.
                       ▼

       Шаг 3: оператор останавливает chiit-client
       ┌─────────────────────────────────────────────────────────┐
       │ Node                                                    │
       │ ┌─────────────────────┐                                 │
       │ │ chiit-client        │   ── systemctl stop ──▶ stopped │
       │ │ (старый key мёртв)  │                                 │
       │ │                     │                                 │
       │ └─────────────────────┘                                 │
       │ ┌─────────────────────┐                                 │
       │ │ bosun-client        │   подключён через Subscribe     │
       │ │ /etc/bosun/         │   к chiit-server, ECDSA по      │
       │ │   client.pem        │   K_bosun_new — всё ОК.         │
       │ │   (K_bosun_new)     │                                 │
       │ └─────────────────────┘                                 │
       └─────────────────────────────────────────────────────────┘
```

Ключевая идея: registration_token валиден столько же сколько в chiit-flow (один shared per-park secret, не ротируется). Если оператор хочет «перерегистрировать N1», он просто доставляет bosun-binary и token, тот сам себя bootstrap'ит. Никаких extra процедур со стороны admin'а.

---

## 8. Соответствие компонентов: chiit → bosun

```
┌─────────────────────────┬─────────────────────────┬─────────────────────────┐
│ Концепт                 │ chiit                   │ bosun                   │
├─────────────────────────┼─────────────────────────┼─────────────────────────┤
│ Server process          │ chiit-server (Go)       │ ТОТ ЖЕ chiit-server (Go)│
│ Server API namespace    │ ChiitServer service     │ BosunAPI service        │
│ Auth registration       │ shared secret + reg.    │ Тот же registration_    │
│                         │ endpoint                │ token                   │
│ Auth runtime            │ ECDSA host/createdAt/sig│ Та же ECDSA + общая     │
│                         │                         │ таблица chiit_validators│
│ Discovery               │ warden (DNS)            │ k8s Service / DNS       │
│ Inventory               │ storage-inventory svc   │ same (через chiit cache)│
│ Vault                   │ chiit-server cache      │ ditto                   │
│ Cert manager            │ chiit cert_manager      │ ditto                   │
│ Reporting               │ HTTP POST report log    │ ReportApplyResult RPC   │
│ Commands push           │ pull-режим (30s cron)   │ push через Subscribe    │
│                         │                         │ stream                  │
│ Rollout                 │ canary_hash% в клиенте  │ server-side IssueRollout│
│                         │                         │ + state machine         │
│ Bundle distribution     │ go:embed в бинарь       │ PG-stored, version'нная │
│ Self-upgrade            │ p2p re-exec             │ нет — apt-package only  │
│ State persistence       │ HTTP audit log + Vault  │ PG + audit_log + Vault  │
└─────────────────────────┴─────────────────────────┴─────────────────────────┘
```
