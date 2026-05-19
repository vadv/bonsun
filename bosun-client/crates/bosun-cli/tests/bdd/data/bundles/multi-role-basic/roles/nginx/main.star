load("@bosun/builtins", "apt", "file", "template")


def configure(inv):
    apt.package(name = "nginx")
    file.content(
        path = "/etc/nginx/nginx.conf",
        contents = template("nginx.conf.j2", inv = inv),
        mode = 0o644,
        owner = "root",
        group = "root",
    )
