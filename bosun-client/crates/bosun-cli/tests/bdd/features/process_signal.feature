@docker @process-signal
Feature: process.signal primitive — отправка allowlist-сигнала процессу
  bosun реализует chiit-кейс «defers.AddCommand(name, pkill ...)» как
  узкий примитив: ограниченный allowlist сигналов (HUP/TERM/INT/USR1/
  USR2/WINCH/PIPE; без KILL/STOP/CONT), без shell, никаких `sh -c`.
  По умолчанию deferred=True — запись попадает в журнал и исполняется
  на следующем replay.

  Scenario: deferred=True кладёт запись в журнал
    Given a fresh container
    And a bundle with manifest:
      """
      process.signal(
          name = "hup-doorman",
          signal = "HUP",
          process_name = "pg_doorman",
      )
      """
    When I apply the bundle
    Then exit code is 0
    # apply enqueue + post-replay attempt 1 → запись осталась с attempt_count=1.
    And the defer journal contains 1 pending entry
    And the defer journal has a pending entry for "hup-doorman"
    And the pending defer for "hup-doorman" has action "command.run"

  Scenario: KILL отвергается — не входит в allowlist
    Given a fresh container
    And a bundle with manifest:
      """
      process.signal(
          name = "blast",
          signal = "KILL",
          process_name = "yes",
          deferred = False,
      )
      """
    When I apply the bundle
    Then exit code is 1

  Scenario: deferred=False синхронно успешен при отсутствующем процессе
    Given a fresh container
    And a bundle with manifest:
      """
      process.signal(
          name = "hup-absent",
          signal = "HUP",
          process_name = "definitely-not-running-process",
          deferred = False,
      )
      """
    When I apply the bundle
    Then exit code is 0

  Scenario: SIGHUP-префикс эквивалентен HUP
    Given a fresh container
    And a bundle with manifest:
      """
      process.signal(
          name = "sighup-alias",
          signal = "SIGHUP",
          process_name = "ghost-not-running",
      )
      """
    When I apply the bundle
    Then exit code is 0
    And the defer journal has a pending entry for "sighup-alias"

  Scenario: По uid отправляется через pkill -u
    Given a fresh container
    And a bundle with manifest:
      """
      process.signal(
          name = "usr1-for-empty",
          signal = "USR1",
          process_user = "nobody",
      )
      """
    When I apply the bundle
    Then exit code is 0
    And the defer journal has a pending entry for "usr1-for-empty"

  Scenario: Имена с path-traversal отвергаются
    Given a fresh container
    And a bundle with manifest:
      """
      process.signal(
          name = "../escape",
          signal = "HUP",
          process_name = "x",
      )
      """
    When I apply the bundle
    Then exit code is 1
