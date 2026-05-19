@docker @runr-service @todo-skip
Feature: runr.service — declarative управление long-running процессом
  bosun управляет runr-сервисами через HTTP API runr daemon:
  state=running запускает, state=stopped останавливает, notify-driven
  restart_on / reload_on ставит запись в журнал defers, replay-фаза
  следующего apply'я дёргает реальный restart.
  Все сценарии @todo-skip: настоящий runr daemon отсутствует в открытых
  исходниках, в test-base.Dockerfile лежит только stub
  (/usr/local/bin/runr-stub) с минимальным набором ответов. Полноценное
  end-to-end покрытие требует встроенного runr-binary в test image — это
  следующая фаза.

  Scenario: создать запущенный сервис
    Given a fresh container
    When I run "/usr/local/bin/runr-stub --port 8010 --bind 127.0.0.1 &" inside the container
    And a bundle with manifest:
      """
      runr.service(name = "echo", state = "running", enable = True)
      """
    When I apply the bundle
    Then exit code is 0
    And output contains "Running"

  Scenario: notify-driven restart кладёт defer
    Given a fresh container
    When I run "/usr/local/bin/runr-stub --port 8010 --bind 127.0.0.1 &" inside the container
    And a bundle with manifest:
      """
      cfg = file.content(path = "/etc/runr/echo.conf", contents = "v1")
      runr.service(name = "echo", state = "running", restart_on = [cfg])
      """
    When I apply the bundle
    Then exit code is 0
    Given a bundle with manifest:
      """
      cfg = file.content(path = "/etc/runr/echo.conf", contents = "v2")
      runr.service(name = "echo", state = "running", restart_on = [cfg])
      """
    When I apply the bundle
    Then exit code is 0
    And the defer journal has a pending entry for "echo"

  Scenario: restart при недоступном runr daemon уходит в Deferred
    Given a fresh container
    # Daemon НЕ запущен — defer-запись должна остаться pending.
    And a bundle with manifest:
      """
      cfg = file.content(path = "/etc/runr/echo.conf", contents = "v1")
      runr.service(name = "echo", state = "running", restart_on = [cfg])
      """
    When I apply the bundle
    Then exit code is 0
    And the defer journal has a pending entry for "echo"
