load("@bosun/builtins", "file", "inventory")

a = inventory.read("inventory/a.yaml")
b = inventory.read("inventory/b.yaml")
m = inventory.merge_keyed(a, b, key = "id")

# Записываем merge-результат в файл — каждая строка `id=pkg`.
lines = []
for record in m["records"]:
    lines.append(record["id"] + "=" + record["pkg"])

file.content(path = "/etc/keyed-result.txt", contents = "\n".join(lines) + "\n")
