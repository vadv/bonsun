load("@bosun/builtins", "template")


def render_service(exec):
    return template("service.j2", exec_path = exec)
