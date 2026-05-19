@docker @defers
Feature: defers journal — at-least-once журнал отложенных действий
  bosun ставит отложенные действия (notify-driven restart, deferred
  process.signal) в `/tmp/bosun-defers/`. Каждая запись — файл с
  `.deferred` (pending) или `.manual_clear` (промоутированный после
  max_attempts). Имя файла кодирует priority и target, что задаёт
  порядок replay.

  Scenario: process.signal deferred=True создаёт ровно одну запись
    Given a fresh container
    And a bundle with manifest:
      """
      process.signal(
          name = "single",
          signal = "HUP",
          process_name = "ghost",
      )
      """
    When I apply the bundle
    Then exit code is 0
    And the defer journal contains 1 pending entry
    # После apply'я post-replay-фаза уже попыталась исполнить запись,
    # pkill против отсутствующего процесса вернул exit 1 → attempt_count
    # инкрементируется до 1. Файл остаётся в журнале как pending.
    And the pending defer for "single" has attempt_count 1

  Scenario: Повторный bundle apply с тем же ресурсом не плодит дубликаты
    Given a fresh container
    # Запускаем процесс, чтобы pkill --signal USR1 sleep отрабатывал успешно
    # (sleep игнорирует USR1) — defer чистится post-replay, без bump'а
    # attempt_count и без двойного файла.
    When I run "nohup sleep 600 > /dev/null 2>&1 &" inside the container
    Then exit code is 0
    Given a bundle with manifest:
      """
      process.signal(
          name = "dedup",
          signal = "USR1",
          process_name = "sleep",
      )
      """
    When I apply the bundle
    Then exit code is 0
    And the defer journal is empty
    When I apply the bundle again
    Then exit code is 0
    And the defer journal is empty

  Scenario: Несколько разных process.signal порождают разные defer-файлы
    Given a fresh container
    And a bundle with manifest:
      """
      process.signal(name = "a", signal = "HUP", process_name = "ghost-a")
      process.signal(name = "b", signal = "USR1", process_name = "ghost-b")
      """
    When I apply the bundle
    Then exit code is 0
    And the defer journal contains 2 pending entries
    And the defer journal has a pending entry for "a"
    And the defer journal has a pending entry for "b"

  Scenario: Pending defer успешно replay'ится при существующем процессе
    Given a fresh container
    # Запускаем долгоиграющий процесс по имени, к которому pkill сможет
    # достучаться через `pkill --signal USR1 <name>` (sleep игнорирует
    # USR1, поэтому процесс продолжает работать).
    When I run "nohup sleep 600 > /dev/null 2>&1 &" inside the container
    Then exit code is 0
    Given a bundle with manifest:
      """
      process.signal(
          name = "replay-me",
          signal = "USR1",
          process_name = "sleep",
      )
      """
    When I apply the bundle
    Then exit code is 0
    # post-replay уже исполнил defer, pkill вернул 0, файл удалён.
    And the defer journal is empty

  Scenario: Failed defer накапливает attempt_count
    Given a fresh container
    And a bundle with manifest:
      """
      process.signal(
          name = "doomed",
          signal = "HUP",
          process_name = "no-such-process-anywhere",
      )
      """
    When I apply the bundle
    Then exit code is 0
    # После apply'я post-replay уже инкрементировал attempt_count до 1.
    And the pending defer for "doomed" has attempt_count 1
    When I apply the bundle again
    Then exit code is 0
    # Второй apply: pre-replay → bump до 2, evaluate → дед-уп (запись
    # уже есть), post-replay → bump до 3 → promotion в manual_clear.
    And the defer journal contains 0 pending entries
    And the defer journal contains 1 manual_clear entry
