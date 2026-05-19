@docker @bundle
Feature: Bundle directory structure
  bosun applies bundles с role/lib/inventory directory layout.
  Каждый сценарий ссылается на готовую фикстуру из
  tests/bdd/data/bundles/<slug>/. Helper копирует директорию в контейнер
  через docker cp и запускает bosun apply / bundle validate.

  @bundle-roles @slow
  Scenario: Multi-role bundle with explicit inventory loading
    Given a fresh container
    And the bundle "multi-role-basic"
    When I apply the bundle with tags "production"
    Then exit code is 0
    And file "/etc/nginx/nginx.conf" contains "worker_processes 8;"

  @bundle-tags
  Scenario: Missing --tags fails fast
    Given a fresh container
    And the bundle "tags-require"
    When I apply the bundle with tags ""
    Then exit code is 3
    And output contains "expected one of"

  @bundle-tags
  Scenario: Wrong tag fails
    Given a fresh container
    And the bundle "tags-require"
    When I apply the bundle with tags "development"
    Then exit code is 3
    And output contains "expected one of"

  @bundle-templates
  Scenario: Cross-module template access is rejected
    Given a fresh container
    And the bundle "cross-module-template"
    When I apply the bundle with tags "bdd"
    Then exit code is 3

  @bundle-templates
  Scenario: template() from manifests/main.star is rejected
    Given a fresh container
    And the bundle "template-from-manifests"
    When I apply the bundle with tags "bdd"
    Then exit code is 3
    And output contains "manifests"

  @bundle-privacy
  Scenario: Private symbol import is rejected
    Given a fresh container
    And the bundle "private-symbol"
    When I apply the bundle with tags "bdd"
    Then exit code is 3

  @bundle-inventory
  Scenario: Inventory merge strategy replace replaces all lists
    Given a fresh container
    And the bundle "merge-replace"
    When I apply the bundle with tags "bdd"
    Then exit code is 0
    And package "delta" is installed

  @bundle-inventory
  Scenario: Inventory merge strategy deep_map_append_list concats
    Given a fresh container
    And the bundle "merge-append"
    When I apply the bundle with tags "bdd"
    Then exit code is 0
    And package "delta" is installed

  @bundle-inventory
  Scenario: Inventory merge without strategy uses bundle.toml default
    Given a fresh container
    And the bundle "merge-default-strategy"
    When I apply the bundle with tags "bdd"
    Then exit code is 0
    And package "delta" is installed

  @bundle-inventory
  Scenario: Null in override removes key
    Given a fresh container
    And the bundle "merge-null-removes-key"
    When I apply the bundle with tags "bdd"
    Then exit code is 0
    And file "/etc/merge-result.txt" contains "keep=original"
    And file "/etc/merge-result.txt" does not contain "drop="

  @bundle-inventory
  Scenario: merge_keyed combines records by key field
    Given a fresh container
    And the bundle "merge-keyed"
    When I apply the bundle with tags "bdd"
    Then exit code is 0
    And file "/etc/keyed-result.txt" contains "left=merged-pkg"
    And file "/etc/keyed-result.txt" contains "middle=middle-pkg"
    And file "/etc/keyed-result.txt" contains "right=right-pkg"

  @bundle-composition
  Scenario: Role to lib to template composition resolves correctly
    Given a fresh container
    And the bundle "role-lib-template"
    When I apply the bundle with tags "bdd"
    Then exit code is 0
    And file "/etc/myunit.service" contains "ExecStart=/bin/true"

  @bundle-validate
  Scenario: bundle validate exits 0 on clean bundle
    Given a fresh container
    And the bundle "validate-clean"
    When I run "bosun bundle validate --bundle /work/bundle --tags=bdd" inside the container
    Then exit code is 0
    And output contains "evaluate OK"

  @bundle-validate
  Scenario: bundle validate exits 3 on missing inventory file
    Given a fresh container
    And the bundle "validate-broken"
    When I run "bosun bundle validate --bundle /work/bundle --tags=bdd" inside the container
    Then exit code is 3
    And output contains "inventory/missing.yaml"

  @bundle-validate @bundle-roles
  Scenario: examples/multi-role-pg validates with staging tag
    Given a fresh container
    And the bundle from "examples/multi-role-pg/bundle"
    When I run "bosun bundle validate --bundle /work/bundle --tags=staging" inside the container
    Then exit code is 0
    And output contains "evaluate OK"
