@docker @bundle
Feature: Bundle directory structure
  bosun applies bundles with role/lib/inventory directory layout.

  @bundle-roles @slow
  Scenario: Multi-role bundle with explicit inventory loading
    Given a fresh container
    And a bundle structure under "/work/bundle":
      | path                                | body                                                                                                                                       |
      | bundle.toml                         | {"name":"multi","version":"0.1.0","requires_bosun":"^0.1","entry":"manifests/main.star","tags":{"production":""},"inv_strategy":"deep_map_replace_list"} |
      | manifests/main.star                 | load("@bosun/builtins", "inventory", "tags") \n load("@roles/nginx", "configure") \n tags.require_one_of("production") \n inv = inventory.read("inventory/base.yaml") \n configure(inv = inv) |
      | inventory/base.yaml                 | service_workers: 8 \n service_user: nginx                                                                                                  |
      | roles/nginx/main.star               | load("@bosun/builtins", "apt", "file", "template") \n def configure(inv): \n     apt.package(name="nginx") \n     file.content(path="/etc/nginx/nginx.conf", contents=template("nginx.conf.j2", inv=inv), mode=0o644, owner="root", group="root") |
      | roles/nginx/templates/nginx.conf.j2 | worker_processes {{ inv.service_workers }};                                                                                                |
    When I apply the bundle with tags "production"
    Then exit code is 0
    And file "/etc/nginx/nginx.conf" content contains "worker_processes 8;"

  @bundle-tags
  Scenario: Missing --tags fails fast
    Given a fresh container
    And a bundle structure under "/work/bundle":
      | path                | body                                                                                                              |
      | bundle.toml         | {"name":"x","version":"0.1.0","requires_bosun":"^0.1","entry":"manifests/main.star"}                              |
      | manifests/main.star | load("@bosun/builtins", "tags") \n tags.require_one_of("production", "staging")                                  |
    When I apply the bundle with tags ""
    Then exit code is 3
    And output contains "expected one of"

  @bundle-tags
  Scenario: Wrong tag fails
    Given a fresh container
    And a bundle structure under "/work/bundle":
      | path                | body                                                                                                              |
      | bundle.toml         | {"name":"x","version":"0.1.0","requires_bosun":"^0.1","entry":"manifests/main.star"}                              |
      | manifests/main.star | load("@bosun/builtins", "tags") \n tags.require_one_of("production", "staging")                                  |
    When I apply the bundle with tags "development"
    Then exit code is 3
    And output contains "expected one of"

  @bundle-templates
  Scenario: Cross-module template access is rejected
    Given a fresh container
    And a bundle structure under "/work/bundle":
      | path                | body                                                                                                                  |
      | bundle.toml         | {"name":"x","version":"0.1.0","requires_bosun":"^0.1","entry":"manifests/main.star","tags":{"bdd":""}}                |
      | manifests/main.star | load("@roles/a", "configure") \n configure()                                                                          |
      | roles/a/main.star   | load("@bosun/builtins", "template") \n def configure(): \n     x = template("@roles/b:foo.j2")                       |
      | roles/b/main.star   | def stub(): \n     pass                                                                                              |
    When I apply the bundle with tags "bdd"
    Then exit code is 3

  @bundle-templates
  Scenario: template() from manifests/main.star is rejected
    Given a fresh container
    And a bundle structure under "/work/bundle":
      | path                | body                                                                                                                  |
      | bundle.toml         | {"name":"x","version":"0.1.0","requires_bosun":"^0.1","entry":"manifests/main.star","tags":{"bdd":""}}                |
      | manifests/main.star | load("@bosun/builtins", "template") \n template("foo.j2")                                                              |
    When I apply the bundle with tags "bdd"
    Then exit code is 3
    And output contains "manifests"

  @bundle-privacy
  Scenario: Private symbol import is rejected
    Given a fresh container
    And a bundle structure under "/work/bundle":
      | path                | body                                                                                                                  |
      | bundle.toml         | {"name":"x","version":"0.1.0","requires_bosun":"^0.1","entry":"manifests/main.star","tags":{"bdd":""}}                |
      | manifests/main.star | load("@roles/a", "_private")                                                                                          |
      | roles/a/main.star   | def _private(): \n     pass                                                                                          |
    When I apply the bundle with tags "bdd"
    Then exit code is 3

  @bundle-inventory
  Scenario: Inventory merge strategy replace replaces all lists
    Given a fresh container
    And a bundle structure under "/work/bundle":
      | path                  | body                                                                                                                                                                                    |
      | bundle.toml           | {"name":"x","version":"0.1.0","requires_bosun":"^0.1","entry":"manifests/main.star","tags":{"bdd":""}}                                                                                  |
      | manifests/main.star   | load("@bosun/builtins", "apt", "inventory") \n a = inventory.read("inventory/a.yaml") \n b = inventory.read("inventory/b.yaml") \n m = inventory.merge(a, b, strategy="replace") \n apt.package(name = m["servers"][0]) |
      | inventory/a.yaml      | servers: ["alpha", "beta", "gamma"]                                                                                                                                                     |
      | inventory/b.yaml      | servers: ["delta"]                                                                                                                                                                      |
    When I apply the bundle with tags "bdd"
    Then exit code is 0
    And package "delta" is installed

  @bundle-inventory
  Scenario: Inventory merge strategy deep_map_append_list concats
    Given a fresh container
    And a bundle structure under "/work/bundle":
      | path                  | body                                                                                                                                                                                                    |
      | bundle.toml           | {"name":"x","version":"0.1.0","requires_bosun":"^0.1","entry":"manifests/main.star","tags":{"bdd":""}}                                                                                                  |
      | manifests/main.star   | load("@bosun/builtins", "apt", "inventory") \n a = inventory.read("inventory/a.yaml") \n b = inventory.read("inventory/b.yaml") \n m = inventory.merge(a, b, strategy="deep_map_append_list") \n apt.package(name = m["servers"][3]) |
      | inventory/a.yaml      | servers: ["alpha", "beta", "gamma"]                                                                                                                                                                     |
      | inventory/b.yaml      | servers: ["delta"]                                                                                                                                                                                      |
    When I apply the bundle with tags "bdd"
    Then exit code is 0
    And package "delta" is installed

  @bundle-inventory
  Scenario: Null in override removes key
    Given a fresh container
    And a bundle structure under "/work/bundle":
      | path                  | body                                                                                                                                                                                                    |
      | bundle.toml           | {"name":"x","version":"0.1.0","requires_bosun":"^0.1","entry":"manifests/main.star","tags":{"bdd":""},"inv_strategy":"deep_map_replace_list"}                                                            |
      | manifests/main.star   | load("@bosun/builtins", "apt", "inventory") \n a = inventory.read("inventory/a.yaml") \n b = inventory.read("inventory/b.yaml") \n m = inventory.merge(a, b) \n apt.package(name = m["keep"])           |
      | inventory/a.yaml      | drop: original \n keep: original                                                                                                                                                                        |
      | inventory/b.yaml      | drop: null                                                                                                                                                                                              |
    When I apply the bundle with tags "bdd"
    Then exit code is 0
    And package "original" is installed

  @bundle-composition
  Scenario: Role to lib to template composition resolves correctly
    Given a fresh container
    And a bundle structure under "/work/bundle":
      | path                              | body                                                                                                                                                                |
      | bundle.toml                       | {"name":"x","version":"0.1.0","requires_bosun":"^0.1","entry":"manifests/main.star","tags":{"bdd":""}}                                                              |
      | manifests/main.star               | load("@roles/myrole", "configure") \n configure()                                                                                                                   |
      | roles/myrole/main.star            | load("@bosun/builtins", "file") \n load("@lib/runr", "render_service") \n def configure(): \n     file.content(path="/etc/myunit.service", contents=render_service(exec="/bin/true")) |
      | _lib/runr/main.star               | load("@bosun/builtins", "template") \n def render_service(exec): \n     return template("service.j2", exec_path=exec)                                              |
      | _lib/runr/templates/service.j2    | ExecStart={{ exec_path }}                                                                                                                                            |
    When I apply the bundle with tags "bdd"
    Then exit code is 0
    And file "/etc/myunit.service" content contains "ExecStart=/bin/true"

  @bundle-validate
  Scenario: bundle validate exits 0 on clean bundle
    Given a fresh container
    And a bundle structure under "/work/bundle":
      | path                | body                                                                                                                  |
      | bundle.toml         | {"name":"x","version":"0.1.0","requires_bosun":"^0.1","entry":"manifests/main.star","tags":{"bdd":""}}                |
      | manifests/main.star | load("@bosun/builtins", "apt", "tags") \n tags.require_one_of("bdd") \n apt.package(name = "nothing")                  |
    When I run "bosun bundle validate --bundle /work/bundle --tags=bdd" inside the container
    Then exit code is 0
    And output contains "evaluate OK"

  @bundle-validate
  Scenario: bundle validate exits 3 on missing inventory file
    Given a fresh container
    And a bundle structure under "/work/bundle":
      | path                | body                                                                                                                  |
      | bundle.toml         | {"name":"x","version":"0.1.0","requires_bosun":"^0.1","entry":"manifests/main.star","tags":{"bdd":""}}                |
      | manifests/main.star | load("@bosun/builtins", "inventory") \n inventory.read("inventory/missing.yaml")                                       |
    When I run "bosun bundle validate --bundle /work/bundle --tags=bdd" inside the container
    Then exit code is 3
    And output contains "inventory/missing.yaml"
