load("@bosun/builtins", "apt", "inventory")

a = inventory.read("inventory/a.yaml")
b = inventory.read("inventory/b.yaml")
m = inventory.merge(a, b, strategy = "replace")
apt.package(name = m["servers"][0])
