load("@bosun/builtins", "inventory", "tags")
load("@roles/postgres", configure_postgres = "configure")
load("@roles/patroni", configure_patroni = "configure")
load("@roles/pgbouncer", configure_pgbouncer = "configure")

tags.require_one_of("production", "staging")

# Глобальный inventory: общие настройки ноды + numbered prefix per-роль.
base = inventory.read("inventory/base.yaml")
pkg_layer = inventory.read("inventory/postgresql/01_packages.yaml")
install_layer = inventory.read("inventory/postgresql/02_install.yaml")
config_layer = inventory.read("inventory/postgresql/03_config.yaml")

inv = inventory.merge(base, pkg_layer, install_layer, config_layer)

# Поверх — окружение.
if tags.has("production"):
    inv = inventory.merge(inv, inventory.read("inventory/production.yaml"))
elif tags.has("staging"):
    inv = inventory.merge(inv, inventory.read("inventory/staging.yaml"))

configure_postgres(inv = inv)
configure_patroni(inv = inv)
configure_pgbouncer(inv = inv)
