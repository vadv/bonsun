load("@bosun/builtins", "apt", "file", "template")


def configure(inv):
    apt.package(name = "patroni")

    file.content(
        path = "/etc/patroni/patroni.yml",
        contents = template("patroni.yml.j2", inv = inv),
        mode = 0o640,
        owner = "postgres",
        group = "postgres",
    )
