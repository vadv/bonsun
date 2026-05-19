@docker @systemd-service @todo-skip
Feature: systemd.service — declarative управление через org.freedesktop.systemd1
  bosun управляет systemd unit'ами через dbus: state, enable, notify-
  driven restart/reload. После restart верифицирует через InvocationID
  diff, что unit реально перезапустился.
  Все сценарии @todo-skip: запуск настоящего systemd как PID 1 в
  контейнере без --privileged невозможен; python3-dbusmock установлен,
  но требует raw system bus setup (dbus-daemon --system + регистрация
  mock на org.freedesktop.systemd1). Это next-step после Phase K — пока
  все сценарии помечены как известные пробелы покрытия.

  Scenario: InvocationID diff verification после restart
    Given a fresh container
    # Поднять system bus + python3-dbusmock manager-template
    And a bundle with manifest:
      """
      systemd.service(name = "nginx", state = "running")
      """
    When I apply the bundle
    Then exit code is 0
    And stdout contains "no-change"

  Scenario: enable_unit gate через is_unit_enabled
    Given a fresh container
    And a bundle with manifest:
      """
      systemd.service(name = "already-enabled", state = "running", enable = True)
      """
    When I apply the bundle
    Then exit code is 0
    And output contains "enable"

  Scenario: NoChange path при matching state
    Given a fresh container
    And a bundle with manifest:
      """
      systemd.service(name = "static-unit", state = "running")
      """
    When I apply the bundle
    Then exit code is 0
    When I apply the bundle again
    Then exit code is 0
    And stdout contains "no-change"
