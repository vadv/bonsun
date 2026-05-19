@docker @runr-service
Feature: runr.service — declarative управление long-running процессом
  bosun управляет runr-сервисами через HTTP API runr daemon:
  state=running запускает, state=stopped останавливает, notify-driven
  restart_on / reload_on ставит запись в журнал defers, replay-фаза
  следующего apply'я дёргает реальный restart.
  Сценарии работают против настоящего runr-supervisor'а, собранного
  в той же bookworm-glibc, что и bosun (см. Makefile target
  `runr-bookworm`). Daemon живёт в фоне контейнера, отвечает на
  127.0.0.1:8010. PID 1 контейнера — по-прежнему `tail -f /dev/null`,
  поэтому init_system fact = `unknown`; сценарий принудительно
  навешивает `--init-system runr` через шаг бутстрапа.

  Scenario: создать запущенный сервис
    Given a fresh container with runr daemon
    And the runr service "echo" with unit file:
      """
      [Service]
      Type=simple
      ExecStart=/bin/sh -c 'while true; do sleep 30; done'
      Restart=always
      """
    And a bundle with manifest:
      """
      runr.service(name = "echo", state = "running", enable = True)
      """
    When I apply the bundle
    Then exit code is 0
    And runr service "echo" is in state "Running"

  Scenario: notify-driven restart исполняется через defer+replay
    Given a fresh container with runr daemon
    And the runr service "echo" with unit file:
      """
      [Service]
      Type=simple
      ExecStart=/bin/sh -c 'while true; do sleep 30; done'
      Restart=always
      """
    And a bundle with manifest:
      """
      cfg = file.content(path = "/etc/runr/echo.conf", contents = "v1")
      runr.service(name = "echo", state = "running", restart_on = [cfg])
      """
    When I apply the bundle
    Then exit code is 0
    And runr service "echo" is in state "Running"
    And the defer journal is empty
    # Поведение runr: счётчик `restarts` инкрементится только при
    # автоматических рестартах (Restart=always после exit/crash), не на
    # внешние API-вызовы restart. Достоверный сигнал «restart реально
    # случился» — PID процесса изменился между апплаями.
    Given I remember pid of runr service "echo" as "before"
    And a bundle with manifest:
      """
      cfg = file.content(path = "/etc/runr/echo.conf", contents = "v2")
      runr.service(name = "echo", state = "running", restart_on = [cfg])
      """
    When I apply the bundle
    Then exit code is 0
    And the defer journal is empty
    And pid of runr service "echo" differs from "before"

  Scenario: недоступный runr daemon переводит ресурс в Outcome::Deferred
    Given a fresh container with runr daemon
    And the runr service "echo" with unit file:
      """
      [Service]
      Type=simple
      ExecStart=/bin/sh -c 'while true; do sleep 30; done'
      Restart=always
      """
    And a bundle with manifest:
      """
      runr.service(name = "echo", state = "running")
      """
    When I apply the bundle
    Then exit code is 0
    And runr service "echo" is in state "Running"
    When I stop the runr daemon
    Given a bundle with manifest:
      """
      runr.service(name = "echo", state = "running")
      """
    When I apply the bundle
    Then exit code is 0
    And output contains "deferred"
    When I start the runr daemon
    Given a bundle with manifest:
      """
      runr.service(name = "echo", state = "running")
      """
    When I apply the bundle
    Then exit code is 0
    And runr service "echo" is in state "Running"
