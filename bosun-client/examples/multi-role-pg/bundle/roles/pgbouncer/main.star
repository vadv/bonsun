load("@bosun/builtins", "apt", "file", "template")


def configure(inv):
    apt.package(name = "pgbouncer")

    file.content(
        path = "/etc/pgbouncer/pgbouncer.ini",
        contents = template("pgbouncer.ini.j2", inv = inv),
        mode = 0o640,
        owner = "postgres",
        group = "postgres",
    )
