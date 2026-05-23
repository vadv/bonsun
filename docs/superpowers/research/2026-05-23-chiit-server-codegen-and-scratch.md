# chiit-server: make generate + scratch auth — research

Date: 2026-05-23

## TL;DR

`make generate` гонит `.proto` через Ozon-овский Mimir-оркестратор, который вызывает `buf` с шестью plugin'ами и на каждый сервис выплёвывает **семь файлов**: `*.pb.go` (типы), `*_grpc.pb.go` (gRPC stubs), `*.pb.gw.go` (REST↔gRPC мост), `*.pb.scratch.go` (proxy + ServiceDesc для регистрации в Scratch-приложении), `*_vtproto.pb.go` (быстрая marshal/unmarshal + pooling), `*.pb.sensitivity.go` (маскирование PII в логах), `*.swagger.json` (OpenAPI v2). Дополнительно генерируются Go-клиент через swagger-codegen и mock'и через minimock.

**Scratch — это Ozon internal Go-фреймворк** (`gitlab.ozon.ru/platform/scratch`, версия pinned `v1.16.34`), сочетающий в себе app-runtime (порты, config, lifecycle), s2s-авторизацию через OAuth2 client-credentials по протоколу OpenID Connect, и платформенную обвязку (tracing, metrics, vault, real-time config). chiit-server **не использует встроенную s2s-аутентификацию scratch для входящих запросов от пользователя** — оператор аутентифицируется одним из двух способов: статический `admin-token`/`validator-registration-token` в gRPC metadata или HTTP-header (источник секретов — Vault), либо JWT от Keycloak (заголовок `x-bearer-token`, имя пользователя сверяется с `itc_admin_users` realtime-config'а). Доверие чiит-клиентов держится на ECDSA-подписи запроса с проверкой публичного ключа из БД.

Для bosun-API мы получаем бесплатно: gRPC server stubs, REST gateway, OpenAPI, fast marshal, PII-маскирование, регистрацию сервиса в одном `scratch.App.Run(...)` — рядом с `ChiitServer` и `PgShardManagerV1`. Переиспользуются: Keycloak JWT-валидация (для веб-UI оператора), `admin-token` middleware (для CI/deploy-инструментов), ECDSA-валидация запросов от bosun-агентов. Streaming-RPC технически работают через scratch (gRPC native), но `RegisterGateway` (REST↔gRPC мост) их пропускает — нужно вызывать stream-методы напрямую через gRPC-порт `:82`.

## make generate

### Команды pipeline

Главный target в `chiit-server/Makefile:527`:

```makefile
generate: .validate-min-go-version bin-deps deps-pb .generate generate-config
```

Что делает каждый шаг:

1. `.validate-min-go-version` (`scratch.mk:601`) — гарантирует `go >= 1.19`.
2. `bin-deps` (`scratch.mk:308`) — устанавливает все генераторы в `./bin/` (см. ниже).
3. `deps-pb` (`scratch.mk:332`) → `.deps-pb` (`scratch.mk:327`) — вызывает `mimir-cli vendor`, который выкачивает все `local_dependencies` и `external_services` из `mimir.yaml` в `./vendor.protogen/` (см. `mimir.yaml:13-29` — там 4 локальные + 9 внешних proto-зависимостей вроде `hallpass`, `storage-inventory`, `dutycalendar`).
4. `.generate` (`scratch.mk:499-525`) — собственно генерация. Команда:
   ```
   $(MIMIR_BIN) generate \
     --config mimir.yaml \
     --buf-bin $(BUF_BIN)
   ```
   Mimir внутри читает `buf.gen.yaml` (`chiit-server/buf.gen.yaml:1-45`), запускает `buf generate` и натравливает на каждый `.proto` шесть plugin'ов (см. таблицу ниже). Сразу после этого удаляются устаревшие `*.pb.scratch.go`, делается `go mod tidy`, и форматирование — `goimports -w ./`.
5. `generate-config` (`scratch.mk:496`) — `scratch generate config -v`. Это не proto, а отдельная штука: сборка `internal/config/const.go` и финального `platform.yaml` для k8s — на основе `realtimeConfig:` секции из `.o3/k8s/values.yaml`. См. комментарий в `scratch.mk:24-32`.

Дополнительный `Makefile:11-15` — генерация mock'ов через `minimock` (НЕ запускается из `generate` target автоматически):

```makefile
generate-mocks: tools
    PATH=`pwd`/bin:$$PATH go generate ./...
```

И отдельно — генерация Go-клиента CLI через swagger-codegen (`Makefile:3-6`):

```makefile
generate_clients:
    rm -rf ./cmd/chiit-server-cli/internal/client
    mkdir -p ./cmd/chiit-server-cli/internal/client
    swagger generate client --name chiit-server-client \
        --target=./cmd/chiit-server-cli/internal/client \
        -f ./api.swagger.json
```

То есть swagger из proto делает второй контур: на нём построен Go-клиент для CLI (`chiit-server-cli/internal/client/`), и теоретически можно сгенерить клиент на любом языке с поддержкой OpenAPI v2.

### Список генерируемых артефактов

На каждый `.proto` (в репо их два: `api/chiit/api.proto` и `api/pg_shard_manager/api.proto`) выходит **семь файлов** в директорию рядом с proto:

| Файл | Plugin | Что внутри | Размер для chiit |
|---|---|---|---|
| `*.pb.go` | `protoc-gen-go` | message типы, getters, reflection descriptors | 3798 строк |
| `*_grpc.pb.go` | `protoc-gen-go-grpc` (точнее vtproto замещает) | `XxxClient`/`XxxServer` interfaces, registration, `XxxServiceDesc` | 1627 строк |
| `*_vtproto.pb.go` | `protoc-gen-go-vtproto` | быстрый marshal/unmarshal/size, pool через `gitlab.ozon.ru/platform/scratch/vtproto/pool`, fast gRPC stubs | 11560 строк |
| `*.pb.gw.go` | `protoc-gen-grpc-gateway` | HTTP→gRPC мост: каждой `option (google.api.http)` соответствует http handler, который дёргает gRPC handler внутри процесса | 2706 строк |
| `*.pb.scratch.go` | `protoc-gen-scratch` | `ChiitServerServiceDesc` с `RegisterGRPC`, `RegisterGateway`, `SwaggerDef`, `WithHTTPUnaryInterceptor`; proxy-обёртка, которая прогоняет HTTP-вызовы через gRPC-interceptor'ы для единого middleware-стека | 531 строка |
| `*.pb.sensitivity.go` | `protoc-gen-sensitivity` | автогенерированный код маскирования полей в логах/трассах — для compliance | 5506 строк |
| `*.swagger.json` | `protoc-gen-openapiv2` | OpenAPI 2.0 description — основа для Swagger UI и `go-swagger`-клиентов | 1943 строки |

Дополнительно один shared:

| Файл | Tool | Назначение |
|---|---|---|
| `swagger.go` | `esc` (`gitlab.ozon.ru/platform/esc@v0.2.1`) | embed swagger.json в бинарник для serve через `/swagger.json` endpoint |

Mock'и (только под `go generate ./...`, не в `make generate`):

| Где | Источник |
|---|---|
| `internal/clients/certificate-manager-gateway/client_mock.go` | `//go:generate minimock` директива в `client.go:23` |
| `internal/app/.../mocks/inventory_client_mock.go` | для proto-client'а из vendor.protogen |
| `internal/app/.../mocks/pg_backup_manager_client_mock.go` | то же |
| `internal/app/.../duty_calendar_mock_test.go` (test-only) | для приватного interface'а |
| `internal/clients/itc/*_mock_test.go` | то же |

CLI-клиент (только под `make generate_clients`):

```
cmd/chiit-server-cli/internal/client/client/  (operations: Build, GetBuilds, ...)
cmd/chiit-server-cli/internal/client/models/  (DTO структуры)
```

### Tools chain

Полная цепочка (`scratch.mk:290-308`):

| Tool | Версия | Назначение |
|---|---|---|
| `mimir-cli` | `scratch-15-rc` | оркестратор: vendor + buf invocation |
| `buf` | `v1.4.0` | непосредственно вызывает plugin'ы |
| `protoc-gen-go` | `v1.36.6` | proto types |
| `protoc-gen-go-grpc` | `v1.2.0` | gRPC stubs |
| `protoc-gen-go-vtproto` | `v0.105.0-patch.1` | fast marshal + pool |
| `protoc-gen-grpc-gateway` | `v2.18.1` | HTTP мост |
| `protoc-gen-openapiv2` | `v2.18.1` | swagger.json |
| `protoc-gen-scratch` | `v0.5.1` | ServiceDesc обёртка |
| `protoc-gen-sensitivity` | `v0.5.1` | PII-маскирование |
| `protoc-gen-hedgereqs` | `v0.3.0` | hedge-requests опция (не используется в chiit) |
| `protoc-gen-go-o3-kafka` | `v2.1.4` | Kafka topics из proto (не используется в chiit) |
| `esc` | `v0.2.1` | embed бинарных данных |
| `goimports` | `v0.19.0` | post-format |
| `scratch` | `v1.16.34` | runtime app + tooling |
| `minimock` | `v3.4.5` | mocks (`Makefile:12`) |
| `go-swagger` | system | CLI-клиент (`Makefile:6`) |

## Scratch auth

### Что это

**Scratch** — Ozon internal Go-фреймворк, аналог `kratos` или `go-zero`: единая точка входа для микросервиса в платформенную экосистему Ozon. Origin: `gitlab.ozon.ru/platform/scratch`, версия `v1.16.34` (видно в `scratch.mk:64`). Доступа к исходникам у нас нет (internal Gitlab), но видны публичные API из импортов и сгенерированный `scratch.mk` со ссылками вроде `https://confluence.ozon.ru/pages/viewpage.action?pageId=227007340` (s2sauth), `https://gitlab.ozon.ru/platform/scratch#экспериментальные-фичи`.

Что scratch даёт:
1. Runtime: `scratch.New()` поднимает приложение, дёргает порты — public HTTP `:80`, gRPC `:82`, admin `:84`, channelz, uploads-port `:9090`, proxy `:8080` (см. `.o3/k8s/values.yaml:5-15`).
2. Config: `app.Config()` → realtime-config из etcd + secrets из Vault (через `gitlab.ozon.ru/platform/scratch/config/secret`).
3. Lifecycle: `closer.Add(...)` для graceful shutdown, ровно один `app.Run(serviceDescs...)`.
4. Middleware: `pkg/mw/grpc` (общий unary interceptor stack), `pkg/mw/s2sauth` (s2s OAuth2-токены), `pkg/mw/discovery` (warden service discovery).
5. Observability: tracing-go, metrics-go, healthcheck (`/ready`, `/live`, `/startup` на debug-порту).
6. Tooling: `scratch generate config`, `scratch ast implement` (генерация stub-handler'ов из proto), `scratch render-makefile-help`.

### Как встроено в chiit-server

Точка входа — `cmd/chiit-server/main.go:17-32`:

```go
a, err := scratch.New()
// ...
itcFabric, err := itc.DecoratedClientFabric(ctx, a, grpc.WithTransportCredentials(insecure.NewCredentials()))
// ...
if err := a.Run(
    chiit_server.NewChiitServer(ctx, a, itcFabric),
    chiit_server1.NewPgShardManagerV1(ctx, a),
); err != nil {
    logger.Fatalf(ctx, "can't run app: %s", err)
}
```

Каждый сервис должен реализовать интерфейс `scratch.ServiceDesc`. Сделать это вручную трудно — поэтому есть `protoc-gen-scratch`, который генерит `NewChiitServerServiceDesc(impl)` (`api.pb.scratch.go:32-34`). Каждый сервис подключается к `app.Run()` через метод `GetDescription() scratch.ServiceDesc` (см. `internal/app/.../chiit/service.go:209-211`):

```go
func (i *Implementation) GetDescription() scratch.ServiceDesc {
    return desc.NewChiitServerServiceDesc(i)
}
```

`ChiitServerServiceDesc` (`api.pb.scratch.go:27-52`) умеет три вещи:
- `RegisterGRPC(s *grpc.Server)` — регистрирует service на gRPC-сервере scratch.
- `RegisterGateway(ctx, mux)` — регистрирует REST handler'ы через grpc-gateway. Внутри, если есть HTTP unary interceptor, оборачивает в `proxyChiitServerServer` — все 32 RPC получают interceptor-chain, как если бы это был обычный gRPC-вызов.
- `SwaggerDef()` — отдаёт embed-нутый `api.swagger.json`.

Realtime-config (через `app.Config()`) — основной канал передачи параметров: размеры кэшей, timeout'ы, токены, флаги. См. `internal/config/getter.go:36-110` — там агрегатор поверх scratch realtime-config.

Vault secrets — через `gitlab.ozon.ru/platform/scratch/config/secret` (`internal/config/getter.go:9`). Все credentials (DB DSN, admin api-keys, cert tokens, silence tokens) грузятся через `secret.GetValue(ctx, key)` — синхронно при создании Implementation.

### Поддерживаемые механизмы

В chiit-server параллельно работают **четыре независимых auth-механизма**, выбранных под разные клиенты:

#### 1. `admin-token` (статический shared secret)

Используется для CI-инструментов и operator CLI. Передаётся как gRPC metadata `admin-token: <value>` либо HTTP-header `admin-token: <value>`.

Реализация — `internal/app/.../chiit/utils.go:16-28`:

```go
func (i *Implementation) checkAdminTokenFromContext(ctx context.Context) error {
    secret, errGetSecret := getSecretFromContext(ctx, config.AdminTokenHeader)
    if errGetSecret != nil {
        return errGetSecret
    }
    keys := strings.Split(i.config.AdminAPIKeys, ",")
    for _, key := range keys {
        if strings.Trim(key, " ") == strings.Trim(*secret, " ") {
            return nil
        }
    }
    return fmt.Errorf("bad api key")
}
```

`AdminAPIKeys` — comma-separated список валидных токенов, читается из Vault (`getter.go:72`, ключ `api_keys`). Сам header — константа `admin-token` в `getter.go:29`.

`getSecretFromContext` (`utils.go:47-57`) сначала пробует gRPC metadata, затем HTTP-headers из `ctx.Value(config.HeaderContextKey)`. Поэтому **один и тот же handler работает и через gRPC, и через REST-gateway** без переписывания auth-кода.

Используется в: `release.go:18`, `update_client_key.go:15`, `delete_client.go:15`, `get_last_report.go:16`, `local_storage.go:202` (HTTP file storage).

#### 2. `x-bearer-token` (JWT от Keycloak — для веб-UI оператора)

Хотя пользователь сказал «веб-морды пока нет», JWT-валидация уже реализована — для оператора, обращающегося через `chiit-server-cli` или внутренний UI.

Реализация — `utils.go:30-45`:

```go
func (i *Implementation) checkBearerTokenFromContext(ctx context.Context) error {
    secret, errGetSecret := getSecretFromContext(ctx, config.BearerTokenHeader)
    if errGetSecret != nil {
        return errGetSecret
    }
    currentUser, errValidate := keycloak.ValidateToken(*secret)
    if errValidate != nil {
        return errValidate
    }
    for _, user := range strings.Split(config.GetValue(ctx, config.ItcAdminUsers).String(), ",") {
        if strings.Trim(user, " ") == strings.Trim(currentUser.GetUsername(), " ") {
            return nil
        }
    }
    return fmt.Errorf("bad itc username")
}
```

Header — `x-bearer-token`. JWT валидируется через `keycloak.ValidateToken`:

```go
// internal/keycloak/keycloak.go:46-53
func ValidateToken(token string) (JWTToken, error) {
    _, jwt, errParse := ParseTokenClaims(jwtPubKey, token)
    if errParse != nil {
        return nil, errParse
    }
    return &jwtToken{username: jwt.PreferredUsername, token: token},
        ValidateKeyCloakToken(jwtPubKey, token, jwt.PreferredUsername)
}
```

Публичный ключ Keycloak'а — embed-нут в бинарник (`keycloak.go:14-16`):

```go
//go:embed jwt/jwt.pem
var jwtPubKey string
```

Pure-функция `ValidateKeyCloakToken` (`internal/keycloak/utils.go:51-64`) проверяет RSA-подпись, валидность JWT и сверяет `preferred_username` против списка из realtime-config `itc_admin_users`. Поддерживаются также cookies (`keycloak.go:71-87`) — это для веб-UI с SSO-логином через Keycloak: при обращении к админ-handler'у автоматически берётся access_token из cookie.

Применяется как fallback после `checkAdminTokenFromContext` — паттерн виден в `release.go:18-22`:

```go
if err := i.checkAdminTokenFromContext(ctx); err != nil {
    if errITC := i.checkBearerTokenFromContext(ctx); errITC != nil {
        return nil, status.Error(codes.PermissionDenied,
            fmt.Sprintf("errors: `%s`, `%s`", err.Error(), errITC.Error()))
    }
}
```

То есть либо CI приходит с `admin-token`, либо живой человек приходит с JWT от Keycloak — обе двери в одну ручку.

#### 3. `validator-registration-token` (один-раз для регистрации чiит-клиента)

Используется в `CreateClient` (см. `api.proto:104-120` — security_requirement). По смыслу — bootstrap-токен: новый сервер при первом старте приходит с этим токеном, отдаёт публичный ключ ECDSA, дальше живёт по подписям. Хранится в Vault под ключом `client_registration_token` (`getter.go:14`, `getter.go:68`).

#### 4. ECDSA-подпись хост-запроса (для bosun-агентов аналог)

Самое интересное для нас — этим способом аутентифицируются установленные на серверах chiit-клиенты (chef-replacement). После регистрации клиент имеет приватный ECDSA-ключ; каждый запрос к чувствительным ручкам сопровождается `(host, createdAt, sign)` тройкой, где `sign` = ECDSA-подпись `sha256(host + ":" + createdAt)`.

Реализация — `internal/validator/validator.go:64-88`:

```go
func (v *validator) Validate(ctx context.Context, host string, createdAt int64, sign string) error {
    maxTime := time.Now().Add(-1 * config.GetValue(ctx, config.ClientMaxDiff).Duration()).Unix()
    if maxTime > createdAt {
        return ErrClientTokenTooOld
    }
    cacheKey := fmt.Sprintf("%s:%d:%s", host, createdAt, sign)
    alreadyInCache, signValidOk := v.validatorCache.Get(cacheKey)
    if signValidOk {
        if alreadyInCache.(bool) {
            return nil
        }
        return ErrClientTokenInvalid
    }
    key, errGetKey := v.keyCache.getKey(ctx, host)
    if errGetKey != nil {
        return errGetKey
    }
    digest := sha256.Sum256([]byte(fmt.Sprintf("%s:%d", host, createdAt)))
    errCheckSign := ecsda.CheckSign(key, digest, sign)
    v.validatorCache.Put(cacheKey, errCheckSign == nil)
    return errCheckSign
}
```

Проверка подписи — `internal/ecsda/sign.go:12-28`:

```go
func CheckSign(key *ecdsa.PublicKey, digest [32]byte, signToValidate string) error {
    signature, errBase64 := base64.StdEncoding.DecodeString(signToValidate)
    // ...
    r.SetBytes(signature[:curveOrderByteSize])
    s.SetBytes(signature[curveOrderByteSize:])
    if ecdsa.Verify(key, digest[:], r, s) {
        return nil
    }
    return ErrSignInvalid
}
```

`keyCache` хранит публичные ключи клиентов в БД (хосты регистрируются через `CreateClient` с привязкой к публичному ключу). Защита от replay-attack — TTL на `createdAt` и кэш проверенных подписей. Используется в `VaultGet`, `GetCert`, `GetRSAPairs` — то есть для выдачи секретов и сертификатов конкретному хосту.

#### Дополнительно: s2sauth (Ozon Service-to-Service)

Это **отдельный механизм для исходящих** запросов от chiit-server к другим Ozon-сервисам (`cert-manager-gw`, `dutycalendar`, `hallpass`, `storage-inventory`). Используется только в `internal/clients/certificate-manager-gateway/client.go`:

```go
// chiit-server/internal/clients/certificate-manager-gateway/client.go:14
"gitlab.ozon.ru/platform/scratch/pkg/mw/s2sauth"
```

Это OAuth2 client-credentials flow с Keycloak SSO (`client.go:75-87`):

```go
clientCreds := clientcredentials.Config{
    ClientID:     s2sauth.BuildClientID(clientID),
    ClientSecret: clientSecret,
    TokenURL:     config.GetValue(ctx, config.OpenidAuthUrl).String() + "/protocol/openid-connect/token",
    Scopes:       []string{s2sauth.BuildClientID(gatewayServiceName)},
}
ts := clientCreds.TokenSource(ctx)
// ...
s2sauth.GRPC().TokenWarmup(ctxToken, ts, gatewayServiceName)
```

Header — `x-o3-service-auth: Bearer <jwt>` (`client.go:65, 127, 168`). В chiit-server для **входящих** s2s он отключён (см. `.o3/k8s/values_local.example.yaml:14-24` — `s2s_auth_requests_verify_grpc: disabled`, `s2s_auth_requests_sign_grpc: disabled` и так далее). То есть платформенный механизм есть, но для chiit его сознательно не включают.

### Поток для оператора

Связь между proto-описанием и runtime-валидацией:

1. В `api.proto` для RPC указывается `security_requirement: {key: "admin-token"}` (например `api.proto:36-52` для `Release`). Это попадает в `api.swagger.json` под `security: [{admin-token: []}]`.
2. Внутри handler'а (`release.go:18`) handler сам зовёт `i.checkAdminTokenFromContext(ctx)`. **Никакого общего middleware/interceptor для auth нет** — каждый RPC проверяет токен явно.
3. Если RPC поддерживает несколько auth-механизмов (как `Release`), они проверяются по очереди через fallback.

Это упрощает реализацию (нет magic'а в interceptor-chain), но требует не забыть про проверку в каждой защищённой ручке. Code review и BDD-сценарии — единственный гарант.

Для CLI (`chiit-server-cli`):

```go
// chiit-server-cli/internal/commands/utils.go:19-21
func (a *authAdminKey) AuthenticateRequest(r runtime.ClientRequest, _ strfmt.Registry) error {
    return r.SetHeaderParam(`admin-token`, viper.GetString("api-key"))
}
```

То есть оператор запускает CLI с `--api-key=<token>` (или через viper config), CLI ходит по HTTP/REST к chiit-server, передавая токен в header.

Для веб-морды (если бы была): браузер → Keycloak SSO → редирект в чiит-server с access_token в cookie → `parseCookie` → `ValidateKeyCloakToken` → проверка `preferred_username` в `itc_admin_users`.

## Переиспользование для bosun-API

### Что получим бесплатно

1. **Полный pipeline кодогенерации без правок Makefile**. Достаточно положить `api/bosun/api.proto` рядом с двумя существующими, добавить `api/bosun/**` в `mimir.yaml:5-9` (paths), и `make generate` сделает все 7 файлов автоматически. Никакой настройки plugin'ов.

2. **OpenAPI v2 + Swagger UI бесплатно**. `api.swagger.json` собирается из аннотаций `(grpc.gateway.protoc_gen_openapiv2.options.openapiv2_operation)` — то есть документация и web-морда для отладки появляется одновременно с proto. CLI на любом языке генерируется через swagger-codegen.

3. **REST↔gRPC мост**. Все unary RPC автоматически получают REST-endpoint через `option (google.api.http)`. Это критично для UI: фронт-end не нужно знать про gRPC. Для bosun-агентов используем native gRPC напрямую (быстрее + streaming).

4. **vtproto fast marshal + pool**. Через `protoc-gen-go-vtproto` все сообщения получают оптимизированный (де)сериализатор и pooling, что важно для часто вызываемых endpoint'ов (Heartbeat, Subscribe-stream — ~10x против стандартного `proto.Marshal`).

5. **Регистрация в одном `app.Run(...)`** через `GetDescription() scratch.ServiceDesc`. См. `main.go:27-32` — третий handler `bosun_server.NewBosunAPI(ctx, a)` встаёт рядом с `ChiitServer` и `PgShardManagerV1`. Один процесс, один порт `:82`, один realtime-config, один Vault.

6. **Tracing/metrics/sensitivity** — приходит в комплекте с scratch + sensitivity plugin'ом. Метрики бизнес-уровня объявляются вручную (`internal/metrics/cert_manager_metrics.go:11-32` — пример).

### Что нужно добавить

1. **Новый proto-файл** `api/bosun/api.proto` с сервисом `BosunAPI` и его ~27 RPC (см. соседний research 2026-05-22-bosun-api-in-chiit-server.md). Обязательно прописывать `(google.api.http)` для unary-методов (REST-альтернатива), плюс `(grpc.gateway.protoc_gen_openapiv2.options.openapiv2_operation)` для security_requirement.

2. **Bosun auth-механизм** — для unary вызовов и при подключении к `Subscribe`-stream'у. Скорее всего комбинация:
   - `Bootstrap` RPC: одноразовый `validator-registration-token` (как `CreateClient`), отдаём ECDSA-keypair.
   - Все остальные ручки: ECDSA-подпись `(host, timestamp, sign)` через переиспользование `validator.Validate` — модуль уже умеет кэш, защиту от replay, key-rotation.
   - Альтернатива для streaming: mTLS на основе выданного клиенту сертификата (но требует поднимать TLS на `:82` — в chiit сейчас insecure).

3. **Streaming RPC `Subscribe`** — это **первый** streaming в chiit-server. Streaming поддерживается gRPC natively через scratch (платформенная обвязка не мешает), НО `grpc-gateway` не умеет server-streaming для REST. То есть REST-альтернативы у `Subscribe` не будет — bosun-клиенты обязаны ходить через gRPC порт `:82` напрямую.

4. **Длинноживущая context'а в handler'е**. Все handler'ы chiit-server unary и short-lived (~миллисекунды). `Subscribe` будет держать ctx'у часами или сутками — нужна аккуратность с tracing (отключить per-call span'ы для долгих stream'ов), heartbeat и graceful shutdown через scratch closer.

5. **Outbound пуш-нотификации серверу**: chiit-агенты только pull'ят. Bosun должен push'ить session-команды агентам через `Subscribe`. Это требует in-memory registry активных stream'ов (`map[host]chan<-Command`) с life-cycle через scratch closer.

## Технические нюансы

### Errors

Все handler'ы возвращают `status.Error(codes.X, msg)` — стандартный gRPC-status. Конвертация ошибок репозитория в gRPC-коды — `build.go:27-38`:

```go
func convertError(err error) error {
    switch {
    case err == nil:
        return nil
    case errors.Is(err, repository.ErrNotFound):
        return status.Error(codes.NotFound, err.Error())
    case errors.Is(err, repository.ErrAlreadyExists):
        return status.Error(codes.AlreadyExists, err.Error())
    default:
        return status.Error(codes.Internal, err.Error())
    }
}
```

`grpc-gateway` сам мапит gRPC-codes в HTTP-статусы (404 для NotFound, 409 для AlreadyExists, 403 для PermissionDenied и так далее). Дополнительной обработки не требуется.

Для bosun-API нужно проектировать собственный set ошибок (`BosunError`-enum в proto), который покрывает domain-level семантику — не достаточно generic gRPC-кодов.

### Tracing

`tracer-go/logger` (`logger.Infof(ctx, ...)`) автоматически инжектит trace-id в логи через ctx'у. Plain logrus в `keycloak.go:9`, `local_storage.go:182` — это анти-паттерн, оставшийся от старого кода. Для bosun-API использовать только `tracer-go/logger`.

OpenTelemetry traces попадают в Jaeger через scratch automatic instrumentation (`otlptracegrpc` indirect dep в `go.mod`).

Для streaming `Subscribe` рекомендуется создавать **один** span на всю session'у (с child-span'ами на каждое сообщение), иначе trace storage захлёбывается.

### Mocks

`minimock` генерится **per-file** через `//go:generate` директиву — нет глобального `mocks` package'а. Паттерн:

```go
//go:generate minimock -i path.Interface -o ./client_mock.go -n ClientMock
```

Mock'и для proto-client'ов (генерируемых интерфейсов вроде `InventoryClient`) лежат в `internal/app/.../mocks/`. Mock'и для собственных interface'ов (private, `dutyCalendarInterface`) лежат в `_mock_test.go` рядом с тестом.

Для bosun-API: писать interface'ы тонкими (один глагол — один method), генерить mock через minimock, и **обязательно** покрывать каждый handler собственным `_test.go` с моками всех внешних зависимостей. Шаблон — `internal/app/.../chiit/set_silence_test.go` и `get_master_of_patroni_cluster_test.go`.

### Streaming

В текущем чiит — нет streaming RPC. Все 32+10 RPC из двух сервисов unary. Это означает:

- Платформенный мониторинг (grafana дашборды, scratch metrics) заточен под request-rate, latency p50/p99. Для долгих stream'ов придётся добавить кастомные метрики: `active_streams_gauge`, `messages_sent_total`, `stream_duration_seconds`.
- Healthcheck'и (`/ready`) только проверяют HTTP-доступность — реальный gRPC server health (особенно с подвисшими stream'ами) надо проверять отдельно. Scratch экспортит `channelz` (`channelzPortDefault` в LDFLAGS, `scratch.mk:68`) — там можно увидеть состояние stream'ов.
- vtproto pool работает с unary запросами; для long-lived stream'ов pooling даёт меньше выгоды, но и не вредит.
- Authentication: gRPC interceptor для unary не работает на stream'ах автоматически. Нужно либо отдельный StreamServerInterceptor (поддерживается scratch через `ChainStreamServer`), либо проверка токена в первом сообщении stream'а вручную. Второй вариант проще — `Subscribe` начинается с `HelloMessage` с подписью.

## Open questions

1. **Поддерживает ли scratch ServiceDesc StreamServerInterceptor?** Сгенерированный `*.pb.scratch.go` имеет только `WithHTTPUnaryInterceptor` (см. `api.pb.scratch.go:55`). Нужно проверить, есть ли `WithStreamInterceptor` в более свежем `protoc-gen-scratch` или придётся регистрировать stream-interceptor через goofy путь.

2. **TLS на gRPC-порту `:82`?** Сейчас chiit-server слушает gRPC insecure (warden обеспечивает service-mesh-уровневый TLS). Для bosun-агентов, которые ходят с **внешних** хостов (не из k8s-кластера), нужно либо TLS termination на ingress/LB, либо включить TLS на самом scratch — последнее требует поддержки в платформе.

3. **Хранение ECDSA-ключей bosun-агентов**. Сейчас чiит-клиенты живут в той же таблице `clients`. Делать отдельную таблицу `bosun_agents` или переиспользовать? Аргумент за общую: один хост может быть chef-replacement-клиентом и bosun-агентом одновременно — пусть один ключ. Аргумент против: разный lifecycle, разные TTL, разные права доступа к ручкам.

4. **`generate-config` после добавления proto**. `scratch generate config` собирает финальный `platform.yaml` для k8s. Что произойдёт, если у нас уже два сервиса в одном бинаре и мы добавляем третий — какие новые порты/конфиги появятся? Возможно, нужно вручную править `.o3/k8s/values.yaml`.

5. **Совместимость sensitivity-маскирования с streaming**. `protoc-gen-sensitivity` генерит маскирование для сообщений в логах/трассах. Для unary это включается на boundary handler'а. Для server-stream, где сообщения отправляются сотни раз — маскирование на каждом sent? Это перформанс-овая дыра, надо мерить.

6. **Использовать `protoc-gen-validate`?** В `bin-deps` (scratch.mk:295) нет `protoc-gen-validate` — то есть валидация request-полей не автоматическая. Хочется ли добавить (правила `validate.rules` прямо в proto) или оставить ручную валидацию в handler'ах?

7. **Mimir и vendor.protogen в CI**. `make generate` тянет vendor.protogen через mimir (требует доступа к Ozon Gitlab). Это работает только внутри Ozon-сетки. Для нашего публичного research-репозитория не воспроизводится — у нас уже есть закомитченные сгенерированные файлы как single source of truth. При реальной интеграции в чiит-server (внутри Ozon Gitlab) это не проблема.
