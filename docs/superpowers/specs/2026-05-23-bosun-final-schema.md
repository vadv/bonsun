# bosun-API: финальная схема (v2 после ревью)

Date: 2026-05-23, ревью интегрировано 2026-05-24
Status: согласовано, к реализации

Это v2 финальной схемы. Включает в себя интеграцию ревью `/tmp/bosun-review.md` (DevOps + DBA + Go + PG performance + архитектор) и решения пользователя по принятию/отказу каждого пункта.

## TL;DR

**bosun-API** — новый gRPC namespace в существующем **chiit-server** (Go). Никакого отдельного процесса, никакого нового deployment'а.

**bosun-client** (Rust SCM-агент) ходит в chiit-server по gRPC поверх TLS (server-side cert) с **ECDSA-подписью** каждого запроса — auth-модель полностью как в chiit. Инициатива у клиента: pull раз в 30s, apply (идемпотентно), report. Сервер может «толкнуть» клиента ускорить следующий pull через Subscribe Kick (feature-flag, в первой canary можно выключить).

**Bundle** — `bytea` в PostgreSQL, immutable (только INSERT). LRU-кеш 5 файлов на pod. Hard size limit 5 MB, enforced на server и в DB.

**Rollout** — server-driven gradual через background worker. Identity rollout'а живёт в `bosun_clients.target_rollout_id`, а не в номере версии. Failure-rate halt по 1-минутному тику cumulative с активного времени rollout'а (без скользящего окна, с учётом пауз). Без commands_queue, без per-host ledger.

**Apply идемпотентен.** Это центральная гарантия. Клиент перед apply сравнивает state на диске с bundle, применяет только diff. Повторный apply того же bundle = noop. Это снимает необходимость в durable outbox для ReportApplyResult.

## Архитектурные принципы

1. **Максимум кэширования, минимум PG hits.** Bundle immutable → LRU без инвалидации. `GetTargetVersion` — primary-key read, без guaranteed write.
2. **Pull-модель.** Клиент инициирует, server только отвечает. State per-host = одна строка `bosun_clients` плюс ссылка на текущий rollout.
3. **Иммутабельность.** Bundle, audit, rollout — INSERT only. У rollout меняется только `state`.
4. **Идемпотентный apply.** bosun apply в безопасности применяет тот же bundle второй раз = noop. Решает проблему потерянных ReportApplyResult без durable outbox.
5. **Тайминги увеличены.** Pull 30s; rollout worker тик 60s; bundle публикуется 10-15 раз/неделю.
6. **Auth как в chiit.** TLS + ECDSA request signatures. Без mTLS. Legacy-ноды reuse `/etc/chiit/client.pem`.
7. **Не оптимизируем под 2-3 года вперёд.** Bundle в bytea — acknowledged анти-паттерн, принят с hard size limit. Через год если 100GB станет проблемой — миграция на S3.

## Топология

```
                  ┌─────────────────────────────────┐
                  │       chiit-server pods         │
                  │  ┌───────────────────────────┐  │
                  │  │  ChiitServer (legacy)     │  │
                  │  │  PgShardManagerV1         │  │
                  │  │  BosunAPI         ← new   │  │
                  │  └───────────────────────────┘  │
                  │       ↓ через pooler            │
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
                  │  bosun_audit                    │
                  └─────────────────────────────────┘
                              ↑
              gRPC + TLS + ECDSA-sig  │  Pull (30s) + Subscribe Kick (опц.)
                              │
                ┌─────────────┴─────────────┐
                │     bosun-client (Rust)   │  × ~40-60k nodes
                │  /etc/bosun/client.pem    │
                │  или /etc/chiit/client.pem │  (legacy reuse)
                └───────────────────────────┘
```

## PostgreSQL schema

```sql
-- ОСНОВНАЯ ТАБЛИЦА. Целая строка на host.
CREATE TABLE bosun_clients (
    host                    TEXT PRIMARY KEY,

    -- desired state
    target_version          BIGINT,                  -- какую версию ему отдать; NULL = «ничего не делай»
    target_rollout_id       UUID,                    -- какой rollout установил этот target (для identity, не для версии)
    target_set_at           TIMESTAMPTZ,             -- когда target изменился (для аналитики, ETA)

    -- последнее успешное применение
    current_version         BIGINT,                  -- последняя версия, которую клиент УСПЕШНО применил
    current_set_at          TIMESTAMPTZ,             -- когда current_version был зафиксирован

    -- последняя попытка (включая failed)
    last_attempted_version  BIGINT,                  -- что клиент пытался применить последний раз
    last_attempt_success    BOOLEAN,                 -- результат последней попытки
    last_attempt_at         TIMESTAMPTZ,
    last_attempt_exit_code  INT,
    last_attempt_error      TEXT,                    -- ограниченный excerpt

    -- liveness
    last_seen_at            TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    bosun_version           TEXT,

    created_at              TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Индекс для оператора: «сколько применили v=N в этом rollout'е»
CREATE INDEX ON bosun_clients(target_rollout_id) WHERE target_rollout_id IS NOT NULL;
CREATE INDEX ON bosun_clients(current_version);
-- Btree на last_seen_at НЕ создаём — index churn от 2k UPDATE/sec бесполезен,
-- редкие admin-query'и просканируют 60k rows достаточно быстро.


-- BUNDLE. Immutable. Каждая публикация — новая строка.
CREATE TABLE bosun_bundles (
    version       BIGSERIAL PRIMARY KEY,
    sha256        BYTEA NOT NULL UNIQUE,             -- octet_length = 32
    blob          BYTEA NOT NULL,                    -- сам tar.gz, hard limit 5 MiB enforced в server
    size_bytes    BIGINT NOT NULL,                   -- = octet_length(blob), денормализация для метрик
    signature     BYTEA NOT NULL,                    -- подпись CI/CD ключом
    tags          TEXT[] NOT NULL,
    published_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    published_by  TEXT NOT NULL                      -- из auth context, не из request body
);


-- POD REGISTRY для consistent hashing redirect.
CREATE TABLE bosun_pods (
    pod_id         TEXT PRIMARY KEY,
    addr           TEXT NOT NULL,                    -- достижим с KVM/physical клиентов, не только из cluster DNS
    started_at     TIMESTAMPTZ NOT NULL,
    last_ping_at   TIMESTAMPTZ NOT NULL,
    draining       BOOLEAN NOT NULL DEFAULT FALSE
);


-- ROLLOUT-ПЛАНЫ. Одна строка на rollout. Per-host ledger ОТСУТСТВУЕТ.
-- Membership зафиксирован в target_snapshot JSONB (resolved host list)
-- и в bosun_clients.target_rollout_id.
CREATE TABLE bosun_rollouts (
    rollout_id          UUID PRIMARY KEY,
    target_version      BIGINT NOT NULL REFERENCES bosun_bundles(version),
    target_spec         JSONB NOT NULL,              -- исходный селектор оператора
    target_snapshot     JSONB NOT NULL,              -- resolved host list на момент IssueRollout (массив строк)
    total_targets       INT NOT NULL,                -- length(target_snapshot), не пересчитывается потом

    over_duration_sec   INT NOT NULL,
    max_failure_rate    REAL NOT NULL,               -- 0..1
    max_batch_size      INT NOT NULL,                -- cap на dispatch за один тик worker'а
    min_evaluated       INT NOT NULL,                -- minimum (succeeded+failed) до того как failure_rate halt включается

    state               TEXT NOT NULL DEFAULT 'pending',
                        -- pending → running → (paused | halted | completed | aborted)
    halt_reason         TEXT,

    issued_at           TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    issued_by           TEXT NOT NULL,               -- из auth context
    started_at          TIMESTAMPTZ,
    paused_at           TIMESTAMPTZ,                 -- момент перехода в paused, NULL когда не на паузе
    total_paused_sec    INT NOT NULL DEFAULT 0,      -- накопленное время в паузе
    halted_at           TIMESTAMPTZ,
    aborted_at          TIMESTAMPTZ,
    completed_at        TIMESTAMPTZ,
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX ON bosun_rollouts(state) WHERE state IN ('pending', 'running', 'paused');


-- LEADER-ELECTION с fencing token.
-- Замёрзший лидер не может сделать UPDATE/INSERT в bosun_clients или
-- bosun_rollouts без проверки своего epoch против актуального в БД.
CREATE TABLE bosun_leader (
    role            TEXT PRIMARY KEY,
    pod_id          TEXT NOT NULL,
    epoch           BIGINT NOT NULL,                 -- увеличивается на каждом перехвате роли
    acquired_at     TIMESTAMPTZ NOT NULL,
    last_heartbeat  TIMESTAMPTZ NOT NULL,
    expires_at      TIMESTAMPTZ NOT NULL
);


-- AUDIT LOG. Все мутирующие действия оператора + критичные события системы.
CREATE TABLE bosun_audit (
    audit_id      BIGSERIAL PRIMARY KEY,
    happened_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    event_type    TEXT NOT NULL,                     -- bosun.rollout.issue, bosun.rollout.halt, …
    actor         TEXT NOT NULL,                     -- из auth context (operator email / CI service account / 'system')
    actor_kind    TEXT NOT NULL,                     -- 'operator' | 'ci' | 'system'
    rollout_id    UUID,
    host          TEXT,
    payload       JSONB NOT NULL                     -- параметры действия + relevant ids
);

CREATE INDEX ON bosun_audit(happened_at);
CREATE INDEX ON bosun_audit(rollout_id) WHERE rollout_id IS NOT NULL;
```

CHECK constraint'ы намеренно не добавляются — validation на стороне server-кода и proto enum'ов. Per-host `bosun_rollout_targets` намеренно не вводится — accounting считается агрегатами над `bosun_clients` с фильтром `target_rollout_id`.

### Audit-events набор

| event_type | actor_kind | когда |
|---|---|---|
| `bosun.bundle.publish` | ci | успешный INSERT в bosun_bundles |
| `bosun.rollout.issue` | operator | INSERT bosun_rollouts (state=pending) |
| `bosun.rollout.pause` | operator | state → paused |
| `bosun.rollout.resume` | operator | state → running после paused |
| `bosun.rollout.abort` | operator | state → aborted |
| `bosun.rollout.halt` | system | worker заставил halted из-за failure_rate |
| `bosun.rollout.complete` | system | succeeded + failed == total_targets |
| `bosun.target.override` | operator | SetTargetVersion (точечный override) |
| `bosun.bootstrap.success` | system | новая нода зарегистрирована |
| `bosun.bootstrap.denied` | system | Bootstrap с невалидным token |

`actor` всегда берётся из auth context (interceptor выставил `subject` после проверки ECDSA-подписи или JWT), никогда из request body.

### Как обновляются поля bosun_clients

| Поле | Обновляется через | Заметки |
|---|---|---|
| `target_version`, `target_rollout_id`, `target_set_at` | `SetTargetVersion` (override, rollout_id = NULL); rollout worker (UPDATE батчем с rollout.rollout_id) | единственные пути |
| `current_version`, `current_set_at` | `ReportApplyResult` с `success = true` | обновляется ровно тогда, когда клиент подтвердил успешный apply |
| `last_attempted_version`, `last_attempt_success`, `last_attempt_at`, `last_attempt_exit_code`, `last_attempt_error` | **каждый** `ReportApplyResult` (включая `success = false`) | success/failure последней попытки, для debug и UI |
| `last_seen_at`, `bosun_version` | `GetTargetVersion`, `ReportApplyResult`, `Subscribe`, `Bootstrap` | UPDATE только если diff: `WHERE last_seen_at < NOW() - INTERVAL '5 minutes' OR bosun_version IS DISTINCT FROM $new` — снимает hot-path index churn |
| `created_at` | `Bootstrap` или lazy insert на первом аутентифицированном RPC | см. ниже |

### Lazy insert для legacy chiit-нод

Bootstrap НЕ вызывается на legacy-нодах (они используют reused ECDSA-ключ). Поэтому в `bosun_clients` для них строки нет. На первом аутентифицированном `GetTargetVersion`/`Subscribe`/`ReportApplyResult` interceptor делает:

```sql
INSERT INTO bosun_clients (host, last_seen_at, bosun_version)
VALUES ($host, NOW(), $bosun_version)
ON CONFLICT (host) DO NOTHING;
```

`target_version` остаётся NULL до тех пор, пока оператор не выкатит rollout на эту ноду или не сделает `SetTargetVersion`.

### Семантика для rollout

Клиент входит в одну из трёх корзин в контексте конкретного rollout'а:

- **succeeded** — `target_rollout_id = $rid AND current_version = $rollout.target_version`.
- **failed** — `target_rollout_id = $rid AND last_attempted_version = $rollout.target_version AND last_attempt_success = false AND current_version IS DISTINCT FROM $rollout.target_version`.
- **pending** — `target_rollout_id = $rid AND` (никакой попытки ещё не было ИЛИ последняя попытка была для другой версии).

Никакой агрегации по `target_version >= N` или `< N` — это была correctness-баг'а в v1, теперь identity rollout'а живёт в `target_rollout_id`.

## API (proto sketch)

```protobuf
service BosunAPI {
  // --- BOOTSTRAP (новые ноды, registration_token из Vault) ---
  rpc Bootstrap(BootstrapIn) returns (BootstrapOut);

  // --- PULL MODEL (TLS + ECDSA-sig) ---
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

  // --- CI / OPERATOR (admin-token или Keycloak JWT) ---
  rpc PublishBundle(PublishBundleIn) returns (PublishBundleOut);   // CI service-account auth
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

`PublishBundle` принимает blob, signature, tags, проверяет sha256, signature, hard size limit (5 MiB), и INSERT'ит в `bosun_bundles`. Доступен только CI service-account'ам (отдельная проверка в auth interceptor).

`GetTargetVersionOut` дополнительно возвращает `pull_jitter_seconds` — клиент при смене target должен подождать рандомное значение в `[0, pull_jitter_seconds]` перед apply, чтобы 1500 hosts не пошли в Bundle download синхронно. Сервер выставляет jitter на основе `max_batch_size` и `over_duration`.

## Lifecycle bosun-client

```
СТАРТ:
  if exists(/etc/bosun/client.pem):
      ecdsa_key = read(/etc/bosun/client.pem)
  elif exists(/etc/chiit/client.pem):
      # БЕСШОВНАЯ МИГРАЦИЯ: тот же ECDSA-ключ обслуживает chiit и bosun.
      # Bootstrap НЕ вызывается. Сервер при первом GetTargetVersion
      # сделает lazy insert в bosun_clients.
      ecdsa_key = read(/etc/chiit/client.pem)
      symlink(/etc/bosun/client.pem → /etc/chiit/client.pem)
  else:
      # новая нода
      token = read(/etc/bosun/registration-token)
      cert = Bootstrap(token, hostname, public_key)
      write(/etc/bosun/client.pem)

PULL-LOOP (каждые 30s ИЛИ при Kick):
  resp = GetTargetVersion(host, current_version, last_attempted_version, sharding_key)
                                                           ↑
                            (last_attempted шлём чтобы сервер увидел, что нас уже накрыло)

  if resp.pod_redirect:
      reconnect to resp.redirect_addr
      restart Subscribe + pull-loop
      continue

  if resp.target_version == 0 or == local current_version:
      continue                              # accounting align'нится без apply

  # jitter перед download/apply
  sleep uniform(0, resp.pull_jitter_seconds)

  manifest = GetBundleManifest(resp.target_version)
  if not cached(manifest.sha256):
      stream = GetBundleBlob(resp.target_version)
      bundle_path = write_chunks_to_tempfile(stream)
      verify_sha256(bundle_path, manifest.sha256)
      verify_signature(bundle_path, manifest.signature)

  # bosun apply ИДЕМПОТЕНТЕН: внутри сравнивает текущий state на диске
  # с тем, что описано в bundle, и применяет только diff. Повторный
  # apply того же bundle (когда state уже соответствует) = noop.
  result = bosun apply --bundle=bundle_path

  ReportApplyResult(
      applied_version = resp.target_version,
      success         = result.ok,
      exit_code       = result.code,
      error_excerpt   = result.stderr_tail
  )

  if result.ok:
      write_local_current_version(resp.target_version)
  # на следующем pull'е current_version отправится в GetTargetVersion;
  # если сервер по какой-то причине не получил наш предыдущий
  # ReportApplyResult (TCP fail, pod restart), accounting align'нется
  # на следующем тике после повторного apply (noop) + повторного report
```

**Что даёт идемпотентный apply.** Если ReportApplyResult не дошёл из-за TCP/restart/network, клиент через 30 секунд снова получит тот же target, снова сделает apply (noop, потому что state уже соответствует bundle) и снова попытается отправить report. Сервер увидит свежий ReportApplyResult — accounting align'нется. Никаких apply_run_id, outbox в /var/lib/bosun, UNIQUE-constraint'ов. Лаг в худшем случае = один pull-цикл (~30s).

**Что НЕ делает клиент:**
- Не делает backoff на failed apply. Failed apply ретраится каждые 30s. GetTargetVersion дешёвый (PK read), повторный apply дешёвый (либо noop, либо тот же failure внутри bundle). Если apply навсегда сломан — оператор увидит failed-count в rollout и сделает rollback явным новым rollout'ом.
- Не хранит outbox для ReportApplyResult.

## Lifecycle оператора (gradual rollout)

```
1. CI публикует bundle:
     PublishBundle(blob, sig, tags) → version=42, size_bytes=...
     INSERT bosun_bundles
     INSERT bosun_audit (event_type='bosun.bundle.publish')

2. Оператор раскатывает (severity-aware, один rollout = один severity):
     bosun rollout --version=42 --severity=low \
                   --over 30m --max-failure-rate 0.05 \
                   --min-evaluated 20 --max-batch 100

     IssueRollout(
        target_version=42,
        Target{severity_class="low"},
        over_duration=30m,
        max_failure_rate=0.05,
        min_evaluated=20,
        max_batch_size=100,
     )

     # resolve target на момент IssueRollout
     resolved_hosts = expand_target(Target{severity_class="low"})  # из inventory
     INSERT bosun_rollouts (
        rollout_id=$rid,
        target_version=42,
        target_spec=Target{...},
        target_snapshot=JSONB(resolved_hosts),
        total_targets=length(resolved_hosts),
        ...,
        state='pending'
     )
     INSERT bosun_audit (event_type='bosun.rollout.issue', rollout_id=$rid)
     возврат rollout_id, total_targets

3. Background worker (один leader через bosun_leader, fencing token) раз в минуту:

   for each running rollout:
     # активное время с учётом пауз
     elapsed_sec = (NOW() - started_at).seconds - total_paused_sec

     # accounting по target_rollout_id (НЕ по >= target_version)
     dispatched = COUNT(target_rollout_id = $rid)
     succeeded  = COUNT(target_rollout_id = $rid
                        AND current_version = $rollout.target_version)
     failed     = COUNT(target_rollout_id = $rid
                        AND last_attempted_version = $rollout.target_version
                        AND last_attempt_success = false
                        AND current_version IS DISTINCT FROM $rollout.target_version)
     evaluated  = succeeded + failed
     pending    = dispatched - evaluated

     # halt только когда есть статистически значимая выборка
     if evaluated >= rollout.min_evaluated
        AND failed / evaluated > rollout.max_failure_rate:
         UPDATE bosun_rollouts SET state='halted',
                                   halted_at=NOW(),
                                   halt_reason='failure_rate=...'
         INSERT bosun_audit (event_type='bosun.rollout.halt', ...)
         continue

     # dispatch батч
     expected = total_targets * elapsed_sec / over_duration_sec
     diff = expected - dispatched
     batch_size = min(diff, rollout.max_batch_size)
     if batch_size > 0:
         hosts = SELECT host FROM bosun_clients
                  WHERE host = ANY(rollout.target_snapshot::text[])
                    AND target_rollout_id IS DISTINCT FROM $rid
                  ORDER BY host  -- детерминированный порядок
                  LIMIT batch_size
                  FOR UPDATE SKIP LOCKED
         UPDATE bosun_clients
            SET target_version = $rollout.target_version,
                target_rollout_id = $rid,
                target_set_at = NOW()
          WHERE host = ANY(hosts)

     # completion
     if evaluated == total_targets:
         UPDATE bosun_rollouts SET state='completed', completed_at=NOW()
         INSERT bosun_audit (event_type='bosun.rollout.complete', ...)

4. (Опционально, если Subscribe feature-flag = on) оператор: bosun kick
     KickRollout → server проходит по live Subscribe-stream'ам,
     отправляет Kick{} матчинговым клиентам → они сразу делают
     GetTargetVersion, не ждут следующий 30s тик.

5. Мониторинг:
     bosun rollout-status <rollout_id>
     → GetRolloutStatus → state, dispatched, succeeded, failed,
                          current_failure_rate, elapsed_active_sec, ETA

6. Управление:
     bosun rollout-pause:
        UPDATE bosun_rollouts SET state='paused', paused_at=NOW()
     bosun rollout-resume:
        UPDATE bosun_rollouts
           SET state='running',
               total_paused_sec = total_paused_sec + (NOW() - paused_at).seconds,
               paused_at = NULL
     bosun rollout-abort:
        UPDATE bosun_rollouts SET state='aborted', aborted_at=NOW()

7. Rollback: новый явный rollout, не auto-revert:
     bosun rollout --version=41 --severity=low --over 30m
     → IssueRollout(target_version=41, ...) → новый rollout_id
     → worker UPDATE'ит bosun_clients.target_version=41, target_rollout_id=новый
     → клиенты в pull-loop'е увидят, скачают, применят (если состояние
       откатываемо — это уже на стороне bundle и DSL)
```

### State machine rollout

```
pending → running                 -- worker впервые увидел, started_at = NOW()
running ⇄ paused                  -- PauseRollout/ResumeRollout, total_paused_sec накапливается
running → halted                  -- evaluated >= min_evaluated AND failed/evaluated > max_failure_rate
halted → running                  -- оператор осознанно ResumeRollout
running | paused | halted → aborted  -- AbortRollout (необратимо)
running → completed               -- evaluated == total_targets
```

### Stalled hosts

Если в rollout'е остаются hosts с `target_rollout_id = $rid` без apply в течение `3 × over_duration_sec` (т.е. ноды не отзываются), оператор может:
- Дождаться и Abort.
- Или сделать `SetTargetVersion(stalled_hosts, target_version = $rollout.target_version)` повторно (no-op, но событие в audit) — это не помогает, нода всё равно молчит, поэтому единственное реальное решение — диагностика отдельно.

Stalled-статус в схеме отдельной колонкой не выделяем, query `WHERE target_rollout_id = $rid AND last_seen_at < NOW() - INTERVAL '15 minutes'` находит таких клиентов.

### Альтернатива rollout'у: точечный override

`SetTargetVersion(hosts=[N1, N2], target_version=41)` — мгновенный UPDATE `bosun_clients` с `target_rollout_id = NULL` (явный сигнал, что это override, не часть какого-то rollout'а). Для emergency / debug одной ноды. Audit: `bosun.target.override`.

## Leader election с fencing

`bosun_leader` — таблица с `expires_at`, `last_heartbeat`, **и epoch (fencing token)**.

`epoch` инкрементируется на каждом перехвате роли (нынешний `epoch + 1` записывается при INSERT нового лидера). Worker перед каждым mutation batch'ем проверяет:

```sql
-- внутри той же tx, что и UPDATE bosun_clients
SELECT epoch INTO db_epoch FROM bosun_leader WHERE role='rollout_worker';
IF db_epoch != $my_epoch THEN
    -- я больше не лидер; freeze это поймал, выходим из batch'а
    ROLLBACK;
    return to candidate loop;
END IF;
-- mutation
UPDATE bosun_clients ...;
INSERT bosun_audit ...;
COMMIT;
```

Это защищает от ситуации: pod-лидер замёрз дольше `expires_at`, сосед перехватил роль (epoch++), старый лидер очухался и попытался продолжить worker-loop. UPDATE отвергнется по epoch mismatch.

### Candidate-цикл (каждые 10s в каждом pod'е)

```sql
BEGIN;
  -- мьютекс на момент UPSERT'а
  IF NOT pg_try_advisory_xact_lock(hashtext('bosun_leader:rollout_worker')) THEN
    ROLLBACK;
    continue;
  END IF;

  SELECT pod_id, epoch, expires_at INTO row FROM bosun_leader
   WHERE role='rollout_worker';

  IF row IS NULL OR row.expires_at < NOW() THEN
    -- свободно или протухло, претендуем
    new_epoch = COALESCE(row.epoch, 0) + 1;
    UPSERT bosun_leader SET pod_id=$self, epoch=new_epoch,
                            acquired_at=NOW(), last_heartbeat=NOW(),
                            expires_at=NOW()+INTERVAL '30 seconds';
    my_epoch = new_epoch;
  ELSIF row.pod_id = $self THEN
    -- я и есть лидер, просто продлеваем
    UPDATE bosun_leader SET last_heartbeat=NOW(),
                            expires_at=NOW()+INTERVAL '30 seconds';
    my_epoch = row.epoch;
  ELSE
    -- чужой активный лидер
    my_epoch = NULL;
  END IF;
COMMIT;
```

## Bootstrap и миграция с chiit

```
┌────────────────────┬────────────────────────────────────────────────┐
│ Нода с chiit       │ Нода без агента                                 │
├────────────────────┼────────────────────────────────────────────────┤
│ есть /etc/chiit/   │ нет /etc/chiit/client.pem                       │
│   client.pem       │                                                 │
├────────────────────┼────────────────────────────────────────────────┤
│ bosun-client при   │ bosun-client при старте:                        │
│ старте:            │   1. Читает /etc/bosun/registration-token       │
│   - reuse ECDSA-   │   2. Bootstrap(token, hostname, public_key)     │
│     ключ как       │   3. Сервер проверяет token в Vault             │
│     bosun.pem      │   4. INSERT в chiit_validators                  │
│     (symlink)      │   5. (НЕ INSERT в bosun_clients — это сделает   │
│   - Bootstrap НЕ    │      первый GetTargetVersion через lazy insert)│
│     вызывается     │   6. Возврат cert (для server-TLS chain) и pub  │
│   - первый аутен-  │      key подпись                                │
│     тифицированный │   7. bosun.pem → ECDSA private key              │
│     GetTargetVer-  │                                                 │
│     sion триггерит │                                                 │
│     lazy INSERT    │                                                 │
│     bosun_clients  │                                                 │
└────────────────────┴────────────────────────────────────────────────┘
```

Auth на всех запросах после Bootstrap (включая legacy reuse) — ECDSA-подпись через тот же `chiit_validators`. Никакого mTLS. Server-side TLS-cert — обычный, как у chiit-server для всего остального трафика.

## Тайминги

| Что | Период | Почему |
|---|---|---|
| Pull (GetTargetVersion) | 30s | heartbeat в нём же |
| Subscribe Kick | event-driven | optional, feature-flag |
| Rollout worker тик | 60s | проверка failure_rate и dispatch батча |
| Leader candidate / heartbeat | 10s | быстро перехватить роль при freeze |
| Leader expires_at | 30s | 3× heartbeat запас |
| Pod last_ping_at | 30s | UPDATE bosun_pods каждый тик |
| Pod mature_threshold | 60s | новый pod не включается в hashing 1-ю минуту |
| Pod drain_timeout | 30s | при SIGTERM ждать graceful drain |
| Bundle LRU | 5 файлов на pod | покрывает 99% запросов |
| last_seen_at UPDATE | по diff (5 min или новая bosun_version) | избежать 2k indexed UPDATE/sec |
| pull_jitter | computed | server возвращает в GetTargetVersionOut |
| stalled threshold | 3 × over_duration_sec | для query'я зависших клиентов |

## Go-структура (chiit-server/internal/bosun/)

Пакетная структура задаётся сразу, чтобы handlers оставались тонкими и rollout-математика была pure/testable.

```
internal/bosun/
├── server.go                 # gRPC service registration, composition
├── auth/
│   └── interceptor.go         # ECDSA-sig verification, admin-token, JWT
├── clients/
│   ├── store.go               # bosun_clients storage (Get/UpsertLazy/UpdateAttempt)
│   └── presence.go            # diff-update логика для last_seen_at
├── rollouts/
│   ├── store.go               # bosun_rollouts storage
│   ├── worker.go              # background worker loop
│   ├── leader.go              # candidate + heartbeat + fencing
│   ├── math.go                # PURE: current_state + counters + clock → next action
│   └── math_test.go
├── bundles/
│   ├── catalog.go             # bosun_bundles INSERT/SELECT
│   ├── publish.go             # PublishBundle handler (sha/sig/size validation)
│   ├── lru.go                 # in-memory cache
│   └── singleflight.go        # GetBundleBlob: один SELECT на pod на bundle
├── pods/
│   ├── registry.go            # bosun_pods + heartbeat
│   └── hashing.go             # Jump Consistent Hash (НЕ crc32 % N)
├── proxy/
│   ├── vault.go               # VaultGet → chiit
│   ├── cert.go                # GetCert → chiit
│   └── inventory.go           # StorageHostInventoryGet → chiit
└── audit/
    └── log.go                 # bosun_audit INSERT helpers
```

## Pod redirect: Jump Consistent Hash

Вместо `crc32(sharding_key) % active_pods` (который reshuffle'ит большую долю клиентов при добавлении/удалении pod'а) используется **Jump Consistent Hash** ([Lamping & Veach, 2014](https://arxiv.org/abs/1406.2294)).

Свойства:
- При изменении количества pod'ов с N на N+1 пересчитывается только ~1/N клиентов (минимальный reshuffle).
- O(log N) compute.
- Не требует хранить ring/sorted-state, работает на чистом hash + counters.

```go
// pseudo
func PodForHost(shardingKey string, numPods int) int {
    return jumpHash(hash64(shardingKey), numPods)
}
```

mature_threshold (60s) применяется как и раньше: новый pod не включается в `numPods` первую минуту, что защищает от storm при crashloop.

## Subscribe: feature-flag

Subscribe stream — это Kick-only канал, ускоряет первый pull после `IssueRollout`. Его можно держать выключенным в первой production canary и принять лишнюю задержку ~30s (один pull-цикл). Это разумное упрощение для первой версии.

Если включаем:
- Hard cap на active streams per pod (config).
- Один stream per host (HostId как идемпотентный idempotency key — старый stream закрывается).
- Bounded per-session channel размера 1 (Kick coalescible).
- gRPC keepalive, max connection age, max message size — настроены явно.
- Метрики `bosun_active_streams`, `bosun_stream_send_failures`.

## Observability (P0 metrics)

```
bosun_get_target_version_requests_total{result}
bosun_get_target_version_db_write_total                  # сколько GetTargetVersion реально дошли до UPDATE
bosun_report_apply_result_total{success}
bosun_rollout_state{rollout_id, state}
bosun_rollout_failure_rate{rollout_id}
bosun_rollout_dispatched_total{rollout_id}
bosun_rollout_evaluated_total{rollout_id, outcome}
bosun_bundle_cache_hit_total{result}
bosun_bundle_download_bytes_total
bosun_active_streams                                     # если Subscribe = on
bosun_pod_redirect_total
bosun_leader_current{role, pod_id}
bosun_auth_failures_total{reason}
bosun_publish_bundle_total{result}
db_query_duration_seconds{family}                        # PG latency histogram per query family
```

## Deployment & migration plan

1. **PG migration.** Apply DDL для всех `bosun_*` таблиц. Никаких данных пока не вставляем.
2. **Deploy chiit-server с `bosun_enabled=false`.** Код есть, RPC отвечают `Unavailable`. Это чтобы убедиться, что новая ветка не сломала legacy chiit RPC.
3. **Enable API в одном environment** (staging/dev). Один pod, один тестовый клиент. Прогон Bootstrap, GetTargetVersion, ReportApplyResult.
4. **Backfill или lazy-create bosun_clients.** Lazy-create уже встроен в interceptor, backfill оставляем как opt-in (одноразовая команда `bosun admin backfill-clients-from-chiit-validators`).
5. **Публикация test bundle через CI.** Реальный путь, не вручную. Verify sha+sig+size.
6. **Canary клиентов** — небольшая часть нод с severity_class=test переключается с chiit-client на bosun-client.
7. **24h observation:** DB writes/sec на bosun_clients, GC pauses, stream count (если Subscribe on), auth failures, bundle cache hit rate, корректность rollout status.
8. **Пошаговое расширение** с возможностью откатить ноды обратно на chiit-client (apt downgrade пакет, оставить /etc/chiit/client.pem).

### K8s requirements

- `replicas >= 3` для chiit-server.
- PDB `minAvailable: 2`.
- termination grace > pod drain_timeout (>= 60s).
- Readiness probe зависит от PG connectivity для PG-зависимых ручек.
- HPA по active streams, memory и latency — не только CPU.
- `bosun_pods.addr` — достижим с KVM/physical клиентов (LoadBalancer/NodePort), не только из cluster DNS.

## Что НЕ делаем

- **Self-upgrade.** bosun не апдейтит сам себя.
- **Secret rotation.** Один shared registration-secret per-park, как в chiit.
- **Bundle update.** Bundle immutable, только новые версии INSERT.
- **PG LISTEN/NOTIFY.** Ненадёжный транспорт.
- **Per-host commands queue.** Apply идёт через pull.
- **bosun_rollout_targets per-host ledger.** Accounting агрегатами над `bosun_clients` с фильтром `target_rollout_id`.
- **CHECK constraint'ы в DDL.** Validation на server-коде.
- **Durable outbox / apply_run_id / UNIQUE на (host, run_id).** Apply идемпотентен, повторный pull-цикл align'нет accounting.
- **Backoff клиента на failed apply.** Ретраи каждые 30s, GetTargetVersion дешёвый.
- **mTLS.** TLS + ECDSA-подписи, как в chiit.
- **DB connection release во время streaming GetBundleBlob.** Не оптимизируем, LRU снимает большую часть нагрузки.
- **Auto-rollback при halt.** Только явный новый rollout с N-1.
- **Bundle archive на S3 / OCI.** PG bytea достаточно, hard limit 5 MiB.
- **Web UI.** Operator через CLI.
- **Bundle upload через CLI оператора.** Только CI/CD автоматизация.
- **Реактивный halt rollout'а на каждый ReportApplyResult.** Halt по 1-минутному тику.

## Сводная таблица решений по ревью

| # | Блокер из ревью | Решение |
|---|---|---|
| 1 | Rollback сломан (target_version >/<) | Беру: identity через target_rollout_id, IS DISTINCT FROM |
| 2 | Cohort не зафиксирован | Беру альтернативу: total_targets + target_snapshot JSONB + target_rollout_id в bosun_clients |
| 3 | last_applied vs attempt | Беру: разделяю current_version и last_attempted_* |
| 4 | Retry storm без backoff | Отвергаю: GetTargetVersion дешёвый, ретраи каждые 30s ок |
| 5 | Legacy nodes без bosun_clients row | Беру: lazy insert на первом аутентифицированном RPC |
| 6 | Pause/resume over-dispatch | Беру: paused_at + total_paused_sec, активный elapsed |
| 7 | Durable ReportApplyResult | Отвергаю: apply идемпотентен, retry pull-цикла align'нет |
| 8 | mTLS vs ECDSA confusion | Беру: TLS + ECDSA как в chiit, Bootstrap НЕ нужен legacy |
| 9 | PublishBundle отсутствует в API | Беру: добавлен P0 с CI auth, sha+sig+size |
| 10 | Audit log open question | Беру: bosun_audit + набор событий, actor из auth context |
| DBA | Btree на last_seen_at | Беру: убрал, diff-update раз в 5 min |
| DBA | CHECK constraints | Отвергаю: validation на server-коде |
| DBA | Bundle hard size limit | Беру: 5 MiB enforced, +size_bytes |
| Perf | Singleflight GetBundleBlob | Беру |
| Perf | DB connection release | Отвергаю: LRU снимает |
| Perf | max_batch_size per tick | Беру |
| Perf | min_evaluated_attempts | Беру |
| Perf | Client jitter | Беру (pull_jitter_seconds в ответе) |
| Arch | Пакетная структура | Беру |
| Arch | Pure rollout math | Беру (rollouts/math.go) |
| Arch | Subscribe feature-flag | Беру (можно начать с off) |
| Arch | Leader fencing token | Беру: epoch в bosun_leader |
| Arch | Jump Consistent Hash | Беру вместо crc32 % N |
| DevOps | Deployment plan | Беру (8 шагов) |
| DevOps | K8s requirements | Беру |
| DevOps | Observability metrics | Беру (минимум 13 метрик) |

## Открытые вопросы (для следующих итераций)

- `sharding_key` семантика: hostname, cluster_name, комбинация? (склоняемся к cluster_name → одна нога кластера на одном pod'е, локальность кластеризованных операций).
- Bundle подпись: общий с chiit ключ CI/CD или новый? (склоняемся к общему).
- Audit retention: 90 дней? 1 год? (зависит от compliance, по умолчанию 1 год).
- Backfill команда `bosun admin backfill-clients-from-chiit-validators` — отдельный go-binary или sub-команда chiit-server?
