load("@bosun/builtins", "inventory", "tags")
load("@roles/pgbouncer", configure_pgbouncer = "configure")

tags.require_one_of("production", "staging")

base = inventory.read("inventory/base.yaml")
if tags.has("production"):
    inv = inventory.merge(base, inventory.read("inventory/production.yaml"))
else:
    inv = inventory.merge(base, inventory.read("inventory/staging.yaml"))

configure_pgbouncer(inv = inv)
