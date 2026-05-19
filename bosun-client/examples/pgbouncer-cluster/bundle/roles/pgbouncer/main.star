load("@bosun/builtins", "apt", "file", "service", "template")
load("@lib/runr", "render_service")


def configure(inv):
    """Развернуть pgbouncer как runr-supervised сервис на одной ноде.

    Порядок:
    1. Установка пакета pgbouncer (apt).
    2. Конфиги pgbouncer.ini и userlist.txt (file.content).
    3. INI-файл runr `.service` для управления процессом (file.content +
       _lib/runr.render_service).
    4. service.unit, который подписан на изменения конфигов через
       reload_on — единственная пересборка cgroup/restart управляется
       defer-журналом.
    """
    apt.package(name = "pgbouncer")

    # Главный конфиг pgbouncer. validate_with запускает `pgbouncer -V -c`
    # на временной копии до replace — синтаксически невалидный INI не
    # попадает в /etc/pgbouncer/pgbouncer.ini.
    pgbouncer_config = file.content(
        path = "/etc/pgbouncer/pgbouncer.ini",
        contents = template("pgbouncer.ini.j2", inv = inv),
        owner = "postgres",
        group = "postgres",
        mode = 0o640,
        validate_with = ["pgbouncer", "-V", "-c", "{new_path}"],
    )

    # Список пользователей с паролями. Не подписан на validate (pgbouncer
    # не валидирует userlist отдельно). Группа postgres получает чтение.
    pgbouncer_users = file.content(
        path = "/etc/pgbouncer/userlist.txt",
        contents = template("userlist.txt.j2", inv = inv),
        owner = "postgres",
        group = "postgres",
        mode = 0o640,
    )

    # Описание процесса для runr: ExecStart + User + Autostart. Сама
    # генерация — в _lib/runr/render_service, шаблон лежит в
    # _lib/runr/templates/service.ini.j2.
    runr_unit = file.content(
        path = "/etc/runr/pgbouncer.service",
        contents = render_service(
            name = "pgbouncer",
            exec_start = "/usr/sbin/pgbouncer /etc/pgbouncer/pgbouncer.ini",
            user = "postgres",
            group = "postgres",
            autostart = True,
            limit_nofile = 65536,
        ),
        owner = "root",
        group = "root",
        mode = 0o644,
    )

    # Управление состоянием через абстрактный диспатчер. На ноде с
    # init_system=runr выбирается runr.service, на systemd — systemd.service.
    # reload_on на pgbouncer.ini → defer reload по факту изменения конфига.
    # restart_on на runr_unit → restart, если поменялся сам unit-файл.
    service.unit(
        name = "pgbouncer",
        state = "running",
        enable = True,
        reload_on = [pgbouncer_config, pgbouncer_users],
        restart_on = [runr_unit],
        depends_on = [runr_unit],
        health_check_cmd = ["pg_isready", "-h", "127.0.0.1", "-p", "6432"],
        health_check_retry = 5,
        health_check_retry_interval_sec = 3,
    )
