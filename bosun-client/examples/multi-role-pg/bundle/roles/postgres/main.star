load("@bosun/builtins", "apt", "file", "template")
load("@lib/runr", "render_service")


def configure(inv):
    # Установка пакета postgres.
    apt.package(name = inv["postgres_package"], version = inv["postgres_package_version"])
    apt.package(name = inv["postgres_contrib_package"])

    # Главный конфиг.
    file.content(
        path = "/etc/postgresql/15/main/postgresql.conf",
        contents = template("postgresql.conf.j2", inv = inv),
        mode = 0o640,
        owner = "postgres",
        group = "postgres",
    )

    # Authentication.
    file.content(
        path = "/etc/postgresql/15/main/pg_hba.conf",
        contents = template("pg_hba.conf.j2", inv = inv),
        mode = 0o640,
        owner = "postgres",
        group = "postgres",
    )

    # systemd unit на runr-обёртке (демонстрирует composition role → lib).
    file.content(
        path = "/etc/systemd/system/postgres-runr.service",
        contents = render_service(
            name = "postgres",
            exec_start = "/usr/lib/postgresql/15/bin/postgres -D " + inv["data_dir"],
        ),
        mode = 0o644,
        owner = "root",
        group = "root",
    )
