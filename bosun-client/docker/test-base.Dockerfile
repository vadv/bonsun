FROM debian:bookworm-slim

# Базовый образ для BDD-сценариев bosun. Все примитивы, которые имитируют
# реальные системные действия (useradd, sysctl, pkill, gpg --show-keys,
# sysctl, sha256sum), требуют присутствия соответствующих утилит. Без них
# scenario падает не на assertion'е, а на «not found», и оператор видит
# не симптом, а помеху.
#
# Состав диктуется Phase K (BDD docker) и формально перечислен в плане:
# - apt/dpkg/gpg → apt.package, apt.key, apt.update_cache.
# - python3 → держатель fcntl-локa /var/lib/dpkg/lock-frontend и парсер
#   defer-файлов в тестах (см. defers.rs::defer_file_content).
# - procps (pkill/ps) → process.signal по имени/uid.
# - passwd-suite (useradd/groupadd/userdel/groupdel + getent) → users.user,
#   users.group; в debian:bookworm-slim они уже есть, оставляем явно
#   для документации зависимостей.
# - sysctl → sysctl.reload (procps содержит).
# - jq → удобный grep по JSON-выводу `bosun status --format json` и по
#   payload defer-файлов в шагах сценария.
# - openssl → проверка cert.tls сертификатов (CN, validity) без
#   зависимости от Rust-парсера.
# - dbus-daemon + python3-dbusmock — заготовка для systemd.service-сценариев,
#   которые требуют поднятого system bus с mock'ом org.freedesktop.systemd1.
#   Без --privileged настоящий systemd в контейнере не запустится, но mock
#   через dbus-daemon --system работает.
# - postgresql-client (psql) — для pg_sql.exec/pg_sql.query сценариев,
#   когда docker-compose поднимает реальный postgres рядом.
# - runr-stub: минимальный shell-скрипт, имитирующий HTTP API runr daemon
#   (см. /usr/local/bin/runr-stub). Не запускается по умолчанию; сценарий
#   запускает его явно. Настоящий runr daemon в открытых исходниках
#   отсутствует, поэтому сценарии runr.* в текущей фазе помечены @todo-skip.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
    ca-certificates curl python3 python3-dbusmock \
    dbus dbus-bin dbus-user-session \
    procps psmisc passwd \
    gnupg2 jq openssl \
    postgresql-client \
 && apt-get clean \
 && rm -rf /var/cache/apt/archives/*
# Намеренно НЕ удаляем /var/lib/apt/lists/* — apt.package сценарии
# зависят от свежего кеша, а apt-get update в чистом контейнере без
# списков занимает ~30 секунд, что превышает дефолтный per-attempt
# timeout (30s) в apt_package::recovery. Сохранение кеша сокращает
# время каждого `bosun apply` с apt.package до миллисекунд.

# runr-stub: минимально совместимый сервер на python3, реализующий
# те endpoint'ы, которые дёргает bosun-runr-client::Client. Используется
# в BDD-сценариях runr.service в режиме «daemon недоступен → defer».
# Поднимать вручную внутри сценария: `runr-stub --port 8010 &`.
COPY <<'EOF' /usr/local/bin/runr-stub
#!/usr/bin/env python3
"""Минимальный мок runr daemon для BDD-сценариев.

Реализует подмножество API, которое реально использует bosun:
- GET  /api/v1/daemon/info
- GET  /api/v1/services/statuses
- GET  /api/v1/timers/statuses
- GET  /api/v1/units
- POST /api/v1/services/<name>/{start,stop,restart,reload}
- POST /api/v1/timers/<name>/{start,stop,enable,disable}
- POST /api/v1/units/reload

Состояние держится в памяти процесса. Между перезапусками теряется,
этого достаточно для проверки notify-driven restart counter'а
внутри одного сценария.
"""
import argparse
import json
import re
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlparse

STATE_LOCK = threading.Lock()
SERVICES = {}
TIMERS = {}


def now_iso():
    import datetime
    return datetime.datetime.utcnow().isoformat() + "Z"


def make_ack(message=None):
    ack = {"action_id": "stub-action", "accepted_at": now_iso()}
    if message:
        ack["message"] = message
    return ack


def get_service(name):
    with STATE_LOCK:
        if name not in SERVICES:
            SERVICES[name] = {
                "name": name,
                "state": "Stopped",
                "restarts": 0,
                "active_since": None,
            }
        return dict(SERVICES[name])


SERVICE_RE = re.compile(r"^/api/v1/services/([^/]+)/(start|stop|restart|reload)$")
TIMER_RE = re.compile(r"^/api/v1/timers/([^/]+)/(start|stop|enable|disable)$")


class Handler(BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):  # pragma: no cover - quiet
        sys.stderr.write("runr-stub: " + fmt % args + "\n")

    def _send_json(self, status, payload):
        body = json.dumps(payload).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        path = urlparse(self.path).path
        if path == "/api/v1/daemon/info":
            self._send_json(200, {"version": "stub-0.1.0", "listen": "127.0.0.1:8010"})
            return
        if path == "/api/v1/services/statuses":
            with STATE_LOCK:
                self._send_json(200, {"services": list(SERVICES.values())})
            return
        if path == "/api/v1/timers/statuses":
            with STATE_LOCK:
                self._send_json(200, {"timers": list(TIMERS.values())})
            return
        if path == "/api/v1/units":
            with STATE_LOCK:
                services = [{"kind": "service", **v} for v in SERVICES.values()]
                timers = [{"kind": "timer", **v} for v in TIMERS.values()]
                self._send_json(200, {"units": services + timers})
            return
        self._send_json(404, {"error": f"unknown path {path}"})

    def do_POST(self):
        path = urlparse(self.path).path
        match = SERVICE_RE.match(path)
        if match:
            name, action = match.group(1), match.group(2)
            with STATE_LOCK:
                svc = SERVICES.setdefault(name, {
                    "name": name, "state": "Stopped", "restarts": 0, "active_since": None,
                })
                if action == "start":
                    svc["state"] = "Running"
                    svc["active_since"] = now_iso()
                elif action == "stop":
                    svc["state"] = "Stopped"
                    svc["active_since"] = None
                elif action == "restart":
                    svc["state"] = "Running"
                    svc["restarts"] += 1
                    svc["active_since"] = now_iso()
                elif action == "reload":
                    pass
            self._send_json(200, make_ack(f"service {name} {action}"))
            return
        match = TIMER_RE.match(path)
        if match:
            name, action = match.group(1), match.group(2)
            with STATE_LOCK:
                tm = TIMERS.setdefault(name, {"name": name, "enabled": False, "active": False})
                if action == "start":
                    tm["active"] = True
                elif action == "stop":
                    tm["active"] = False
                elif action == "enable":
                    tm["enabled"] = True
                elif action == "disable":
                    tm["enabled"] = False
            self._send_json(200, make_ack(f"timer {name} {action}"))
            return
        if path == "/api/v1/units/reload":
            self._send_json(200, make_ack("units reloaded"))
            return
        self._send_json(404, {"error": f"unknown path {path}"})


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=8010)
    ap.add_argument("--bind", default="127.0.0.1")
    args = ap.parse_args()
    server = ThreadingHTTPServer((args.bind, args.port), Handler)
    print(f"runr-stub listening on {args.bind}:{args.port}", file=sys.stderr)
    server.serve_forever()


if __name__ == "__main__":
    main()
EOF
RUN chmod +x /usr/local/bin/runr-stub

WORKDIR /work
