load("@bosun/builtins", "file")
load("@lib/runr", "render_service")


def configure():
    file.content(
        path = "/etc/myunit.service",
        contents = render_service(exec = "/bin/true"),
    )
