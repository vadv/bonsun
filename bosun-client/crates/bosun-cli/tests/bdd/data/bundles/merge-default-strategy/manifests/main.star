load("@bosun/builtins", "apt", "inventory")

a = inventory.read("inventory/a.yaml")
b = inventory.read("inventory/b.yaml")
m = inventory.merge(a, b)
apt.package(name = m["servers"][3])
