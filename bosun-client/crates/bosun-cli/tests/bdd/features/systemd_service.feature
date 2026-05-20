@docker @systemd-service @systemd-privileged
Feature: systemd.service — declarative управление через org.freedesktop.systemd1
  bosun управляет systemd unit'ами через dbus: state, enable, notify-
  driven restart/reload. После restart верифицирует через InvocationID
  diff, что unit реально перезапустился.
  Сценарии работают против настоящего systemd-as-PID1 в privileged
  контейнере (debian:bookworm-slim, systemd 252). Запускаются только
  через `make test-bdd-systemd`; обычный `make test-bdd` фильтрует
  @systemd-privileged.

  Scenario: первый apply запускает unit (state=running)
    Given a fresh container with systemd
    And the systemd unit "bdd-sleeper" with content:
      """
      [Unit]
      Description=BDD sleeper unit (managed by bosun)

      [Service]
      Type=simple
      ExecStart=/bin/sh -c 'while true; do sleep 30; done'
      Restart=on-failure

      [Install]
      WantedBy=multi-user.target
      """
    And a bundle with manifest:
      """
      systemd.service(name = "bdd-sleeper.service", state = "running", enable = True)
      """
    When I apply the bundle
    Then exit code is 0
    And systemd unit "bdd-sleeper" is in state "active"
    And systemd unit "bdd-sleeper" is enabled

  Scenario: NoChange path при matching state
    Given a fresh container with systemd
    And the systemd unit "bdd-static" with content:
      """
      [Unit]
      Description=BDD static unit

      [Service]
      Type=simple
      ExecStart=/bin/sh -c 'while true; do sleep 30; done'
      """
    And the systemd unit "bdd-static" is started
    And the systemd unit "bdd-static" is enabled
    And a bundle with manifest:
      """
      systemd.service(name = "bdd-static.service", state = "running", enable = True)
      """
    When I apply the bundle
    Then exit code is 0
    And stdout contains "no-change"
    And systemd unit "bdd-static" is in state "active"

  Scenario: notify-driven restart меняет InvocationID
    Given a fresh container with systemd
    And the systemd unit "bdd-notify" with content:
      """
      [Unit]
      Description=BDD notify-restart unit

      [Service]
      Type=simple
      ExecStart=/bin/sh -c 'while true; do sleep 30; done'
      """
    And the systemd unit "bdd-notify" is started
    # Первый apply кладёт config, дёргать restart нечего: defer пустой.
    And a bundle with manifest:
      """
      cfg = file.content(path = "/etc/bdd-notify.conf", contents = "v1")
      systemd.service(name = "bdd-notify.service", state = "running", restart_on = [cfg])
      """
    When I apply the bundle
    Then exit code is 0
    And systemd unit "bdd-notify" is in state "active"
    # Снимок InvocationID между апплаями. Restart-вызов сменит ID, и
    # bosun проверит это через verify_invocation_change.
    Given I remember InvocationID of systemd unit "bdd-notify" as "before"
    And a bundle with manifest:
      """
      cfg = file.content(path = "/etc/bdd-notify.conf", contents = "v2")
      systemd.service(name = "bdd-notify.service", state = "running", restart_on = [cfg])
      """
    # Replay defer'а на второй apply: restart исполняется, InvocationID
    # меняется. Третий apply нужен, чтобы defer-replay прокрутил очередь
    # после enqueue на втором.
    When I apply the bundle
    Then exit code is 0
    When I apply the bundle
    Then exit code is 0
    And systemd unit "bdd-notify" is in state "active"
    And InvocationID of systemd unit "bdd-notify" differs from "before"
