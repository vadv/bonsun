load("@bosun/builtins", "apt", "file", "template")

# nginx ставится без пина версии — apt подберёт то, что предлагает текущий
# bookworm-mirror. Если нужно пинить — задайте `version` явно (например,
# из inv.nginx_version) и убедитесь, что версия доступна в apt-cache madison.
apt.package(
    name = "nginx",
)

# nginx.conf рендерится из шаблона; `worker_processes` берётся из
# defaults/main.yaml, `server_name` подставляется из факта hostname.
file.content(
    path     = "/etc/nginx/nginx.conf",
    contents = template("nginx.conf.j2"),
    mode     = 0o644,
    owner    = "root",
    group    = "root",
)
