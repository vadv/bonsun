load("@bosun/builtins", "apt", "tags")

tags.require_one_of("bdd")
apt.package(name = "nothing")
