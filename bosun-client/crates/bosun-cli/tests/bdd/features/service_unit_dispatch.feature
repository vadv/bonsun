@docker @service-unit
Feature: service.unit dispatcher по факту init_system
  service.unit — абстракция над systemd.service / runr.service: по факту
  init_system выбирается соответствующий примитив. Init-специфичные
  параметры (cgroup_procs_path для runr, condition_path_exists для
  systemd) отклоняются: power-user должен звать конкретный примитив.
  Эти сценарии используют `bosun bundle validate --facts fixture.json`
  и не зависят от реального состояния init-системы.

  Scenario: Dispatch в systemd.service на systemd-ноде
    Given a fresh container
    And facts fixture init_system = "systemd"
    And a bundle with manifest:
      """
      service.unit(name = "nginx", state = "running")
      """
    When I validate the bundle
    Then exit code is 0
    And stdout contains "evaluate OK"

  Scenario: Dispatch в runr.service на runr-ноде
    Given a fresh container
    And facts fixture init_system = "runr"
    And a bundle with manifest:
      """
      service.unit(name = "pgbouncer", state = "running")
      """
    When I validate the bundle
    Then exit code is 0
    And stdout contains "evaluate OK"

  Scenario: На mixed-systemd-runr ноде primary = systemd.service
    Given a fresh container
    And facts fixture init_system = "mixed-systemd-runr"
    And a bundle with manifest:
      """
      service.unit(name = "nginx", state = "running")
      """
    When I validate the bundle
    Then exit code is 0
    And stdout contains "evaluate OK"

  Scenario: init-специфичный параметр cgroup_procs_path отвергается
    Given a fresh container
    And facts fixture init_system = "systemd"
    And a bundle with manifest:
      """
      service.unit(
          name = "nginx",
          state = "running",
          cgroup_procs_path = "/sys/fs/cgroup/system.slice/nginx.service/cgroup.procs",
      )
      """
    When I validate the bundle
    Then exit code is 3
    And stderr contains "unexpected keyword argument"

  Scenario: init-специфичный параметр condition_path_exists отвергается
    Given a fresh container
    And facts fixture init_system = "runr"
    And a bundle with manifest:
      """
      service.unit(
          name = "nginx",
          state = "running",
          condition_path_exists = "/etc/nginx/nginx.conf",
      )
      """
    When I validate the bundle
    Then exit code is 3
    And stderr contains "unexpected keyword argument"

  Scenario: init_system факт unknown — диспатчер падает
    Given a fresh container
    And facts fixture init_system = "unknown"
    And a bundle with manifest:
      """
      service.unit(name = "nginx", state = "running")
      """
    When I validate the bundle
    Then exit code is 3
    And stderr contains "unsupported init_system"

  Scenario: init_system факт отсутствует — диспатчер падает
    Given a fresh container
    And a bundle with manifest:
      """
      service.unit(name = "nginx", state = "running")
      """
    When I validate the bundle without facts fixture
    Then exit code is 3
    And stderr contains "init_system"
