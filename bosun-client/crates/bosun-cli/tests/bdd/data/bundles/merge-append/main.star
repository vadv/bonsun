load("@bosun/builtins", "apt", "inventory")

a = inventory.read("inventory/a.yaml")
b = inventory.read("inventory/b.yaml")
m = inventory.merge(a, b, strategy = "deep_map_append_list")
apt.package(name = m["servers"][3])
