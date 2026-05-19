@docker @file-delete
Feature: file.delete primitive
  bosun снимает с диска файлы, симлинки и директории по указанному пути.
  Семантика — узкая: если объекта уже нет, повторный запуск возвращает
  no-change. Симлинк удаляется как символическая ссылка, без следования за
  ней — это защищает реальные данные от случайного rm в bundle'е.

  Scenario: Существующий файл удаляется
    Given a fresh container
    When I run "echo seed > /tmp/to-delete.txt" inside the container
    Then exit code is 0
    Given a bundle with manifest:
      """
      file.delete(path = "/tmp/to-delete.txt")
      """
    When I apply the bundle
    Then exit code is 0
    And file "/tmp/to-delete.txt" does not exist in container

  Scenario: Повторное удаление — no-change
    Given a fresh container
    When I run "echo seed > /tmp/twice.txt" inside the container
    Then exit code is 0
    Given a bundle with manifest:
      """
      file.delete(path = "/tmp/twice.txt")
      """
    When I apply the bundle
    Then exit code is 0
    When I apply the bundle again
    Then exit code is 0
    And stdout contains "no-change"

  Scenario: Удаление отсутствующего файла — сразу no-change
    Given a fresh container
    And a bundle with manifest:
      """
      file.delete(path = "/tmp/absent.txt")
      """
    When I apply the bundle
    Then exit code is 0
    And stdout contains "no-change"

  Scenario: Удаление симлинка не следует за целью
    Given a fresh container
    When I run "echo target > /tmp/target.txt && ln -s /tmp/target.txt /tmp/link" inside the container
    Then exit code is 0
    Given a bundle with manifest:
      """
      file.delete(path = "/tmp/link")
      """
    When I apply the bundle
    Then exit code is 0
    And file "/tmp/link" does not exist in container
    And file "/tmp/target.txt" exists in container

  Scenario: Удаление непустой директории требует recursive=True
    Given a fresh container
    When I run "mkdir -p /tmp/dir && echo x > /tmp/dir/inside.txt" inside the container
    Then exit code is 0
    Given a bundle with manifest:
      """
      file.delete(path = "/tmp/dir")
      """
    When I apply the bundle
    Then exit code is 1
    And file "/tmp/dir" exists in container

  Scenario: Удаление директории c recursive=True работает
    Given a fresh container
    When I run "mkdir -p /tmp/dir2 && echo x > /tmp/dir2/inside.txt" inside the container
    Then exit code is 0
    Given a bundle with manifest:
      """
      file.delete(path = "/tmp/dir2", recursive = True)
      """
    When I apply the bundle
    Then exit code is 0
    And file "/tmp/dir2" does not exist in container
