load("@bosun/builtins", "template")


def render_service(
        name,
        exec_start,
        user = "",
        group = "",
        working_directory = "",
        environment = [],
        exec_reload = "",
        autostart = True,
        restart = "always",
        timeout_sec = "",
        restart_sec = "",
        limit_nofile = 0,
        max_memory_rss = "",
        kill_mode = "control-group"):
    """Сгенерировать INI-файл runr `.service` юнита.

    Аргументы соответствуют полям секции [Service] из формата runr (см.
    postgres-chiit/lib/runr/types.go::ServiceSection). Опциональные поля
    передаются строкой/числом со значением по умолчанию, при котором они
    не попадают в финальный текст INI. Это позволяет шаблону работать в
    Strict-режиме minijinja: каждый ключ всегда определён в `inv`.
    """
    return template(
        "service.ini.j2",
        name = name,
        exec_start = exec_start,
        user = user,
        group = group,
        working_directory = working_directory,
        environment = environment,
        exec_reload = exec_reload,
        autostart = autostart,
        restart = restart,
        timeout_sec = timeout_sec,
        restart_sec = restart_sec,
        limit_nofile = limit_nofile,
        max_memory_rss = max_memory_rss,
        kill_mode = kill_mode,
    )


def render_timer(name, on_calendar = "", on_startup_sec = "", unit_name = "", autostart = True, randomized_delay_sec = ""):
    """Сгенерировать INI-файл runr `.timer` юнита.

    Должен быть задан хотя бы один из `on_calendar` или `on_startup_sec`
    (валидация на стороне runr). `unit_name` — имя целевого `.service`;
    по умолчанию runr берёт имя таймера.
    """
    return template(
        "timer.ini.j2",
        name = name,
        on_calendar = on_calendar,
        on_startup_sec = on_startup_sec,
        unit_name = unit_name,
        autostart = autostart,
        randomized_delay_sec = randomized_delay_sec,
    )


def render_cgroup(name, memory_max = "", cpu_max = "", io_max = []):
    """Сгенерировать INI-файл runr `.cgroup` юнита.

    `name` раскрывается runr'ом в `/sys/fs/cgroup/<name>`. `memory_max`
    принимает суффиксы `K/M/G` (база 1024). `cpu_max` задаётся в процентах
    одного ядра: `"50%"` → 0.5 CPU, `"250%"` → 2.5 CPU.
    """
    return template(
        "cgroup.ini.j2",
        name = name,
        memory_max = memory_max,
        cpu_max = cpu_max,
        io_max = io_max,
    )
