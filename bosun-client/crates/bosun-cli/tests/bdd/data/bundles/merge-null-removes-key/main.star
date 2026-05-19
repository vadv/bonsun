load("@bosun/builtins", "file", "inventory")

a = inventory.read("inventory/a.yaml")
b = inventory.read("inventory/b.yaml")
m = inventory.merge(a, b)

# Если бы ключ "drop" остался, мы бы получили строку "original\noriginal\n".
# Раз он удалён, в файле будет только значение "keep".
content = ""
if "drop" in m:
    content += "drop=" + m["drop"] + "\n"
content += "keep=" + m["keep"] + "\n"

file.content(path = "/etc/merge-result.txt", contents = content)
