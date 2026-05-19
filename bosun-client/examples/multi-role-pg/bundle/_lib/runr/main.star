load("@bosun/builtins", "template")


def render_service(name, exec_start, restart = "always", user = "postgres"):
    """Сгенерировать systemd unit-файл для runr-supervised сервиса."""
    return template(
        "service.j2",
        name = name,
        exec_start = exec_start,
        restart = restart,
        user = user,
    )
