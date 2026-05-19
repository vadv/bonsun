@docker @bosun-status
Feature: bosun status — инспектирование журнала defer'ов
  Команда `bosun status` показывает текущее содержимое `/tmp/bosun-defers/`:
  pending (`.deferred`) и promoted (`.manual_clear`). Exit-коды:
  0 — журнал пуст или содержит только pending; 1 — есть manual_clear,
  оператору надо разбираться.

  Scenario: Пустой журнал — "no pending defers" и exit 0
    Given a fresh container
    And the defer journal is empty
    When I run "bosun status"
    Then exit code is 0
    And stdout contains "no pending defers"

  Scenario: Pending defer показывается в табличном выводе
    Given a fresh container
    And the defer journal has a pending entry for "nginx" with action "restart"
    When I run "bosun status"
    Then exit code is 0
    And stdout contains "nginx"
    And stdout contains "pending"

  Scenario: Manual_clear возвращает exit 1
    Given a fresh container
    And the defer journal has a manual_clear entry for "pgbouncer" with action "reload"
    When I run "bosun status"
    Then exit code is 1
    And stdout contains "pgbouncer"
    And stdout contains "manual_clear"

  Scenario: JSON-формат выдаёт валидный массив
    Given a fresh container
    And the defer journal has a pending entry for "nginx" with action "restart"
    When I run "bosun status --format json"
    Then exit code is 0
    And stdout contains "target"
    And stdout contains "nginx"
    And stdout contains "state"
    And stdout contains "pending"

  Scenario: --clear удаляет конкретную запись
    Given a fresh container
    And the defer journal has a pending entry for "to-clear" with action "restart"
    When I run "bosun status --clear systemd.restart:to-clear"
    Then exit code is 0
    When I run "bosun status"
    Then exit code is 0
    And stdout contains "no pending defers"

  Scenario: Pending + manual_clear — exit 1, обе строки в выводе
    Given a fresh container
    And the defer journal has a pending entry for "live-svc" with action "restart"
    And the defer journal has a manual_clear entry for "dead-svc" with action "restart"
    When I run "bosun status"
    Then exit code is 1
    And stdout contains "live-svc"
    And stdout contains "dead-svc"
