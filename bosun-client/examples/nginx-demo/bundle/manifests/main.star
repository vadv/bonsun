load("@bosun/builtins", "inventory", "tags")
load("@roles/nginx", configure_nginx = "configure")

tags.require_one_of("production", "staging")

inv = inventory.read("inventory/base.yaml")
if tags.has("production"):
    inv = inventory.merge(inv, inventory.read("inventory/production.yaml"))
elif tags.has("staging"):
    inv = inventory.merge(inv, inventory.read("inventory/staging.yaml"))

configure_nginx(inv = inv)
