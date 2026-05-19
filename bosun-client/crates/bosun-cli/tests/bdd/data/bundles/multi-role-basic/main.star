load("@bosun/builtins", "inventory", "tags")
load("@roles/nginx", "configure")

tags.require_one_of("production")

inv = inventory.read("inventory/base.yaml")
configure(inv = inv)
