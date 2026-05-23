# bosun-API: финальная схема

Date: 2026-05-23
Status: согласовано в диалоге, к реализации

Это сборка решений за день 23 мая. Заменяет собой все промежуточные итерации (push-модель, commands_queue, sidecar и т.п.) — финальная версия дизайна, идущая в реализацию.

## TL;DR

**bosun-API** — новый gRPC namespace внутри существующего **chiit-server** (Go). Никакого отдельного процесса, никакого нового deployment'а.

**bosun-client** (Rust SCM-агент) ходит в chiit-server по gRPC mTLS, инициатива всегда у клиента: он сам пулит свою целевую версию bundle, сам скачивает, сам применяет, сам репортит результат. Сервер только отвечает на запросы и опционально «толкает» клиента ускорить следующий pull.

**Bundle** хранится в PostgreSQL `bytea`, immutable (только INSERT, никогда UPDATE/DELETE). В каждом pod'е chiit-server'а — LRU-кеш на 5 последних bundle'ов.

**Rollout** — server-driven постепенный с failure-rate halt. Фоновый worker раз в минуту расширяет cohort через `UPDATE bosun_clients.target_version` на батч, считает actual failure rate, при превышении переходит в halted. Без commands_queue.

**Auth** — ECDSA-ключи в общей с chiit-агентами таблице. Bosun-client при старте reuse'ит существующий `/etc/chiit/client.pem` — Bootstrap идёт только для новых нод.

## Архитектурные принципы

1. **Максимум кэширования на сервере, минимум походов в PG.** Bundle immutable → можно кэшировать в pod'е без инвалидации. LRU 5 файлов на pod покрывает 99% запросов.
2. **Pull-модель, не push.** Клиент инициирует — серверу не нужно держать состояние очереди команд per-host. Server-side state per-host = одна строка в `bosun_clients`.
3. **Иммутабельность данных.** Bundle — INSERT only. Audit — INSERT only. Rollout — INSERT only, изменяется только `state`.
4. **Тайминги увеличены.** Полл каждые 30s, heartbeat в нём же. Это абсолютно нормально для SCM-агента, который катит 10-15 раз в неделю.
5. **Бесшовная миграция с chiit.** Один и тот же ECDSA-ключ обслуживает обе системы; bosun-client при старте reuse'ит chiit-ключ если есть.
6. **На этапе разработки не думаем про 2-3 года вперёд.** Bundle в bytea acknowledged как анти-паттерн, но принят. Через год если 100GB станет проблемой — мигрируем на S3.

## Топология

```
                  ┌─────────────────────────────────┐
                  │       chiit-server pods         │
                  │  ┌───────────────────────────┐  │
                  │  │  ChiitServer (legacy)     │  │
                  │  │  PgShardManagerV1         │  │
                  │  │  BosunAPI         ← new   │  │
                  │  └───────────────────────────┘  │
                  │           ↓ через pooler        │
                  └─────────────────────────────────┘
                              │
                  ┌─────────────────────────────────┐
                  │    PostgreSQL (chiit shared)    │
                  │  chiit_validators (shared auth) │
                  │  bosun_clients                  │
                  │  bosun_bundles  (bytea blob)    │
                  │  bosun_pods                     │
                  │  bosun_rollouts                 │
                  │  bosun_leader                   │
                  └─────────────────────────────────┘
                              ↑
                   gRPC mTLS  │  Pull (every 30s) + Subscribe stream (Kick only)
                              │
                ┌─────────────┴─────────────┐
                │     bosun-client (Rust)   │  × ~40-60k nodes
                │  /etc/bosun/client.pem    │
                │  или /etc/chiit/client.pem │  (legacy, bosun reuse'ит)
                └───────────────────────────┘
```

## PostgreSQL schema

```sql
-- ОСНОВНАЯ ТАБЛИЦА. Всё про конкретного клиента — в одной строке.
CREATE TABLE bosun_clients (
    host                  TEXT PRIMARY KEY,
    target_version        BIGINT,           -- какую версию ему отдать; NULL = «ничего не делай»
    last_applied_version  BIGINT,           -- что клиент реально прокатил (по ReportApplyResult)
    last_applied_success  BOOLEAN,          -- true/false из ReportApplyResult, NULL = ещё ничего
    last_applied_at       TIMESTAMPTZ,
    last_seen_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),  -- любой контакт
    bosun_version         TEXT,             -- репортнутая версия бинаря
    created_at            TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX ON bosun_clients(target_version, last_applied_version);
CREATE INDEX ON bosun_clients(last_seen_at);


-- BUNDLE. Immutable. Каждая публикация — новая строка.
CREATE TABLE bosun_bundles (
    version       BIGSERIAL PRIMARY KEY,
    sha256        BYTEA NOT NULL UNIQUE,
    blob          BYTEA NOT NULL,           -- сам bundle tar.gz, max ~5MB
    signature     BYTEA NOT NULL,           -- подпись CI/CD ключом
    tags          TEXT[] NOT NULL,
    published_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    published_by  TEXT NOT NULL
);


-- POD REGISTRY для consistent hashing redirect.
CREATE TABLE bosun_pods (
    pod_id         TEXT PRIMARY KEY,
    addr           TEXT NOT NULL,
    started_at     TIMESTAMPTZ NOT NULL,
    last_ping_at   TIMESTAMPTZ NOT NULL,
    draining       BOOLEAN NOT NULL DEFAULT FALSE
);


-- ROLLOUT-ПЛАНЫ. Одна строка на rollout. Без per-host записей.
CREATE TABLE bosun_rollouts (
    rollout_id          UUID PRIMARY KEY,
    target_version      BIGINT NOT NULL REFERENCES bosun_bundles(version),
    target_spec         JSONB NOT NULL,    -- сериализованный Target (hosts/clusters/severity)
    over_duration_sec   INT NOT NULL,
    max_failure_rate    REAL NOT NULL,     -- 0..1
    state               TEXT NOT NULL DEFAULT 'pending',
                        -- pending → running → (paused | halted | completed | aborted)
    halt_reason         TEXT,
    issued_at           TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    issued_by           TEXT NOT NULL,
    started_at          TIMESTAMPTZ,
    halted_at           TIMESTAMPTZ,
    completed_at        TIMESTAMPTZ
);

CREATE INDEX ON bosun_rollouts(state) WHERE state IN ('pending', 'running');


-- LEADER-ELECTION для фоновых ролей. Источник истины = expires_at + heartbeat.
-- Гонка двух кандидатов защищена pg_try_advisory_xact_lock (transaction-scoped).
CREATE TABLE bosun_leader (
    role            TEXT PRIMARY KEY,         -- 'rollout_worker', будущие роли сюда же
    pod_id          TEXT NOT NULL,
    acquired_at     TIMESTAMPTZ NOT NULL,
    last_heartbeat  TIMESTAMPTZ NOT NULL,
    expires_at      TIMESTAMPTZ NOT NULL
);
```

Никаких bosun_heartbeats, bosun_active_sessions, bosun_commands_queue.

### Как обновляются поля bosun_clients

| Поле | Обновляется через | Заметки |
|---|---|---|
| `target_version` | `SetTargetVersion` (точечный override), rollout worker (UPDATE батчем) | единственные пути |
| `last_applied_version`, `last_applied_success`, `last_applied_at` | **только** `ReportApplyResult` | клиент подтвердил факт apply |
| `last_seen_at` | `GetTargetVersion`, `ReportApplyResult`, `Subscribe`, `Bootstrap` | любой контакт с клиентом |
| `bosun_version` | `GetTargetVersion`, `ReportApplyResult`, `Bootstrap` | репортнутая клиентом версия бинаря |
| `created_at` | `Bootstrap` | только при INSERT новой ноды |

**Семантика для rollout:** клиент, не вызвавший ReportApplyResult, не входит ни в succeeded, ни в failed — только в pending. Heartbeat НЕ интерпретируется как «наверное успешно применил».

## API (proto sketch)

```protobuf
service BosunAPI {
  // --- BOOTSTRAP (registration_token из Vault) ---
  rpc Bootstrap(BootstrapIn) returns (BootstrapOut);

  // --- PULL MODEL (ECDSA auth) ---
  rpc GetTargetVersion(GetTargetVersionIn) returns (GetTargetVersionOut);
  rpc Subscribe(SubscribeIn) returns (stream Kick);
  rpc ReportApplyResult(ReportApplyResultIn) returns (ReportApplyResultOut);
  rpc GetBundleManifest(GetBundleManifestIn) returns (GetBundleManifestOut);
  rpc GetBundleBlob(GetBundleBlobIn) returns (stream BundleChunk);

  // --- AGENT-FACING reuse из chiit ---
  rpc VaultGet(VaultGetIn) returns (.chiit.VaultOut);
  rpc GetCert(GetCertIn) returns (.chiit.CertificateData);
  rpc StorageHostInventoryGet(StorageHostInventoryGetIn)
      returns (StorageHostInventoryGetOut);

  // --- OPERATOR (admin-token или Keycloak JWT) ---
  rpc SetTargetVersion(SetTargetVersionIn) returns (SetTargetVersionOut);
  rpc IssueRollout(IssueRolloutIn) returns (IssueRolloutOut);
  rpc GetRolloutStatus(GetRolloutStatusIn) returns (GetRolloutStatusOut);
  rpc PauseRollout(RolloutControlIn) returns (RolloutControlOut);
  rpc ResumeRollout(RolloutControlIn) returns (RolloutControlOut);
  rpc AbortRollout(RolloutControlIn) returns (RolloutControlOut);
  rpc KickRollout(KickRolloutIn) returns (KickRolloutOut);
  rpc GetClientState(GetClientStateIn) returns (GetClientStateOut);
  rpc CountByVersion(CountByVersionIn) returns (CountByVersionOut);
}
```

Полный proto-файл с message'ами — `docs/superpowers/specs/2026-05-23-bosun-api-p0-proto.md`.

## Lifecycle bosun-client

```
Старт:
  if exists(/etc/bosun/client.pem):
      key = read(/etc/bosun/client.pem)
  elif exists(/etc/chiit/client.pem):
      # БЕСШОВНАЯ МИГРАЦИЯ: тот же приватный ключ обслуживает chiit и bosun
      key = read(/etc/chiit/client.pem)
      write(/etc/bosun/client.pem, key)
  else:
      # новая нода
      token = read(/etc/bosun/registration-token)
      Bootstrap(token, hostname, public_key) → cert
      write(/etc/bosun/client.pem)

Subscribe stream:
  один gRPC server-stream на жизнь процесса
  на отвал — exp backoff reconnect

Pull-loop (каждые 30s ИЛИ при Kick):
  resp = GetTargetVersion(host, current_version, sharding_key)

  if resp.pod_redirect:
      reconnect to resp.redirect_addr
      restart Subscribe + pull-loop
      continue

  if resp.target_version == 0 or == current_version:
      continue

  manifest = GetBundleManifest(resp.target_version)
  if cached(manifest.sha256):
      bundle_path = cache_path(manifest.sha256)
  else:
      stream = GetBundleBlob(resp.target_version)
      bundle_path = write_chunks_to_tempfile(stream)
      verify_sha256(bundle_path, manifest.sha256)
      verify_signature(bundle_path, manifest.signature)

  result = bosun apply --bundle=bundle_path
  ReportApplyResult(
      applied_version = resp.target_version,
      success         = result.ok,
      exit_code       = result.code,
      error_excerpt   = result.stderr_tail
  )
```

## Lifecycle оператора (gradual rollout)

```
1. CI публикует bundle:
     PublishBundle(blob, sig, tags) → version=42

2. Оператор раскатывает (severity-aware, один rollout = один severity):
     bosun rollout --version=42 --severity=low \
                   --over 30m --max-failure-rate 0.05

     IssueRollout(target_version=42,
                  Target{severity_class="low"},
                  over_duration=30m, max_failure_rate=0.05)
     INSERT bosun_rollouts (state='pending')
     возврат rollout_id, total_targets=1500

3. Background worker (один leader через bosun_leader) раз в минуту
   на каждом running rollout:

     dispatched = COUNT(target_version >= rollout.target_version
                        AND host IN cohort)
     succeeded  = COUNT(last_applied_version = rollout.target_version
                        AND last_applied_success = true
                        AND host IN cohort)
     failed     = COUNT(last_applied_version = rollout.target_version
                        AND last_applied_success = false
                        AND host IN cohort)
     pending    = dispatched - succeeded - failed
     failure_rate = failed / (succeeded + failed)         -- 0 если знам. = 0

     -- считаем cumulative с rollout.started_at, без скользящего окна

     if failure_rate > rollout.max_failure_rate:
         UPDATE bosun_rollouts SET state='halted', halted_at=NOW()
         -- новые батчи target_version не апдейтятся
         -- уже dispatched клиенты доедут до новой версии — не откатываем
         continue

     expected = total_targets * elapsed_seconds / over_duration_sec
     diff = expected - dispatched
     if diff > 0:
         hosts = SELECT host FROM bosun_clients
                 WHERE host IN cohort
                   AND (target_version IS NULL OR target_version < rollout.target_version)
                 LIMIT diff
         UPDATE bosun_clients SET target_version = rollout.target_version
                              WHERE host IN (hosts)

     if succeeded + failed == total_targets:
         UPDATE bosun_rollouts SET state='completed', completed_at=NOW()

4. (Опционально) Оператор: bosun kick --severity=low
     KickRollout → server проходит по live Subscribe-stream'ам,
     отправляет Kick{} матчинговым клиентам → они сразу
     делают GetTargetVersion, не ждут следующий 30s тик.

5. Мониторинг:
     bosun rollout-status <rollout_id>
     → GetRolloutStatus → state, dispatched, succeeded, failed,
                          current_failure_rate, ETA

6. Управление:
     bosun rollout-pause   → PauseRollout   → state='paused'
     bosun rollout-resume  → ResumeRollout  → state='running'
     bosun rollout-abort   → AbortRollout   → state='aborted'

7. Rollback: НЕ через auto-revert. Только явный новый rollout:
     bosun rollout --version=41 --severity=low --over 30m
     → IssueRollout(target_version=41, ...)
     → клиенты в pull-loop'е увидят target=41, скачают, применят
```

### State machine rollout

```
pending → running                 -- worker подхватил, started_at = NOW()
running → paused → running        -- оператор PauseRollout/ResumeRollout
running → halted                  -- failure_rate > max
halted → running                  -- оператор осознанно ResumeRollout
running | paused | halted → aborted  -- оператор AbortRollout (необратимо)
running → completed               -- succeeded + failed == total_targets
```

### Альтернатива rollout'у: точечный override

`SetTargetVersion(host=N1, target_version=41)` — мгновенный UPDATE одной (или нескольких) строк bosun_clients, минуя rollout state-machine. Для emergency / debug одной ноды. Не для массового rollback.

## Leader election

`bosun_leader` — таблица с `expires_at` и `last_heartbeat`. `pg_try_advisory_xact_lock` используется как короткий transaction-scoped mutex на момент UPSERT'а (безопасен через pooler — лок живёт до COMMIT и освобождается автоматически).

### Candidate-цикл (каждые 10s в каждом pod'е)

```sql
BEGIN;
  IF NOT pg_try_advisory_xact_lock(hashtext('bosun_leader:rollout_worker')) THEN
    ROLLBACK;            -- сосед сейчас претендует, ждём следующий тик
    continue;
  END IF;

  SELECT pod_id, expires_at INTO row FROM bosun_leader WHERE role='rollout_worker';

  IF row IS NULL OR row.expires_at < NOW() OR row.pod_id = $self_pod THEN
    INSERT INTO bosun_leader (role, pod_id, acquired_at, last_heartbeat, expires_at)
    VALUES ('rollout_worker', $self_pod, NOW(), NOW(), NOW() + INTERVAL '30 seconds')
    ON CONFLICT (role) DO UPDATE
      SET pod_id = EXCLUDED.pod_id,
          acquired_at = CASE WHEN bosun_leader.pod_id = EXCLUDED.pod_id
                              THEN bosun_leader.acquired_at  -- продление, не новый старт
                              ELSE EXCLUDED.acquired_at
                          END,
          last_heartbeat = EXCLUDED.last_heartbeat,
          expires_at = EXCLUDED.expires_at;
    -- self стал/остался лидером
  ELSE
    -- чужой лидер активен, отступаем
    null;
  END IF;
COMMIT;
```

### Heartbeat лидера (каждые 10s)

```sql
UPDATE bosun_leader
   SET last_heartbeat = NOW(),
       expires_at = NOW() + INTERVAL '30 seconds'
 WHERE role = 'rollout_worker' AND pod_id = $self_pod;
-- если 0 rows updated → лидерство потеряно, обратно в candidate-режим
```

### Поведение под нагрузкой

- Лидер заморожен / убит / pod уехал в drain: heartbeat не идёт → expires_at истекает через 30s → сосед на следующем candidate-тике захватывает роль.
- xact_lock защищает от race двух кандидатов: второй блокируется до конца tx первого, увидит уже обновлённый pod_id и отступит.
- PRIMARY KEY на `role` гарантирует что одновременно две строки на одну роль не появятся.

## Bootstrap и миграция с chiit

```
┌────────────────┬────────────────────────────────────────────────────┐
│ Нода с chiit   │ Нода без ни одного агента                          │
├────────────────┼────────────────────────────────────────────────────┤
│ есть           │ нет /etc/chiit/client.pem                          │
│ /etc/chiit/    │                                                    │
│   client.pem   │                                                    │
├────────────────┼────────────────────────────────────────────────────┤
│ bosun-client   │ bosun-client при старте:                           │
│ при старте:    │   1. Читает /etc/bosun/registration-token          │
│   reuse'ит     │      (положен Ansible'ом или cloud-init'ом)        │
│   приватный    │   2. Bootstrap(token, hostname, public_key)        │
│   ключ как     │   3. Сервер проверяет token в Vault, генерит cert  │
│   bosun'овский │   4. INSERT в chiit_validators                     │
│   (тот же     │   5. INSERT в bosun_clients (host, last_seen_at)   │
│    ECDSA, та  │   6. Возврат cert + chain                           │
│    же запись  │   7. write(/etc/bosun/client.pem)                  │
│    в chiit_   │                                                    │
│    validators)│                                                    │
│   Bootstrap   │                                                    │
│   НЕ вызыва-  │                                                    │
│   ется        │                                                    │
└────────────────┴────────────────────────────────────────────────────┘
```

Эффект: миграция с chiit на bosun — операция вида «выкатить bosun-deb-пакет, выключить chiit-сервис». Ни один ECDSA-ключ не пересоздаётся, ни один agent registration не повторяется.

## Тайминги

| Что | Период | Почему |
|---|---|---|
| Pull (GetTargetVersion) | 30s | heartbeat в нём же; rollout катит 10-15 раз/неделю — нет смысла чаще |
| Subscribe Kick | event-driven | оператор хочет ускорить раскатку — server проходит по live стримам |
| Rollout worker тик | 60s | проверка failure_rate и dispatch новых батчей |
| Leader candidate / heartbeat | 10s | быстро перехватить роль если pod упал |
| Leader expires_at | 30s | 3× heartbeat запас на freeze |
| Pod last_ping_at | 30s | UPDATE bosun_pods каждый тик |
| Pod mature_threshold | 60s | новый pod не включается в consistent hashing первую минуту (защита от crashloop) |
| Pod drain_timeout | 30s | при SIGTERM ждать сколько-то для graceful drain |
| Bundle LRU | 5 файлов на pod | покрывает 99% запросов (старая+новая+несколько rollback-кандидатов) |

## Что НЕ делаем

- **Self-upgrade.** bosun не апдейтит сам себя. Обновление = apt-пакет + центральный push, это server-side concern. См. `feedback_bosun_no_self_upgrade.md`.
- **Secret rotation.** Один shared registration-secret per-park, как в chiit. Lifecycle такой же.
- **Bundle update.** Bundle immutable, только новые версии INSERT'ом. Никакого retracted_at, soft-delete и т.п.
- **PG LISTEN/NOTIFY.** Ненадёжный транспорт по опыту DBA. Заменён polling'ом и Subscribe Kick.
- **Per-host commands queue.** Apply идёт через pull, не через push push push.
- **SELECT FOR UPDATE на rollout.** PRIMARY KEY + UPSERT хватает.
- **Auto-rollback при halt rollout'а.** Откат не тривиален и не безопасен (patroni кворум, secret уже применён, runr-задачи запущены). Если нужно — оператор делает новый rollout с N-1.
- **Bundle archive на S3 / OCI.** Bundle живёт в PG bytea. На этапе разработки достаточно. Через год если 100GB станет проблемой — миграция.
- **Web UI.** Нет. Operator через CLI с admin-token / Keycloak JWT.
- **Bundle upload через CLI оператора.** Только CI/CD автоматизация.
- **Реактивный halt rollout'а на каждый ReportApplyResult.** Halt по 1-минутному тику, чтобы не флапать.
- **CountByVersion с детальным breakdown по severity внутри одного RPC.** Каждый severity — отдельный rollout.

## Открытые вопросы (для следующих итераций)

- Audit log: отдельная таблица или INSERT в общий chiit-audit? (склоняемся к общему)
- Sharding_key семантика: что считать «локальностью» для consistent hashing — hostname, cluster_name, или комбинация?
- Bundle подпись: какой ключ CI/CD использует — общий с chiit или новый?
- Когда стартует expires_at у нового rollout'а: при INSERT или когда worker впервые подхватит? (склоняемся: started_at = первый момент когда worker увидел running)
