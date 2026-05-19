@docker @file-symlink
Feature: file.symlink primitive
  bosun создаёт, обновляет и удаляет симлинки. Поведение узкое: цель
  симлинка не canonicalize'ится, симлинк на несуществующий путь —
  валиден (типичный chiit-паттерн «положить симлинк до раскатки
  дистрибутива»).

  Scenario: Создание симлинка на отсутствующую цель
    Given a fresh container
    And a bundle with manifest:
      """
      file.symlink(path = "/usr/local/bin/pg", target = "/opt/pg17/bin/pg")
      """
    When I apply the bundle
    Then exit code is 0
    When I run "test -L /usr/local/bin/pg" inside the container
    Then exit code is 0

  Scenario: Повторное создание — no-change
    Given a fresh container
    And a bundle with manifest:
      """
      file.symlink(path = "/usr/local/bin/pg", target = "/opt/pg17/bin/pg")
      """
    When I apply the bundle
    Then exit code is 0
    When I apply the bundle again
    Then exit code is 0
    And stdout contains "no-change"

  Scenario: Обновление цели симлинка
    Given a fresh container
    And a bundle with manifest:
      """
      file.symlink(path = "/usr/local/bin/pg", target = "/opt/pg16/bin/pg")
      """
    When I apply the bundle
    Then exit code is 0
    Given a bundle with manifest:
      """
      file.symlink(path = "/usr/local/bin/pg", target = "/opt/pg17/bin/pg")
      """
    When I apply the bundle
    Then exit code is 0
    When I run "readlink /usr/local/bin/pg" inside the container
    Then stdout contains "/opt/pg17/bin/pg"

  Scenario: Без force замена существующего файла отвергается
    Given a fresh container
    When I run "echo data > /usr/local/bin/already" inside the container
    Then exit code is 0
    Given a bundle with manifest:
      """
      file.symlink(path = "/usr/local/bin/already", target = "/opt/pg17/bin/pg")
      """
    When I apply the bundle
    Then exit code is 1
    When I run "test -L /usr/local/bin/already" inside the container
    Then exit code is 1

  Scenario: С force=True существующий файл подменяется симлинком
    Given a fresh container
    When I run "echo data > /usr/local/bin/forced" inside the container
    Then exit code is 0
    Given a bundle with manifest:
      """
      file.symlink(path = "/usr/local/bin/forced", target = "/opt/pg17/bin/pg", force = True)
      """
    When I apply the bundle
    Then exit code is 0
    When I run "test -L /usr/local/bin/forced" inside the container
    Then exit code is 0

  Scenario: Удаление симлинка через state=absent
    Given a fresh container
    When I run "ln -s /tmp/x /tmp/link-to-remove" inside the container
    Then exit code is 0
    Given a bundle with manifest:
      """
      file.symlink(path = "/tmp/link-to-remove", target = "/tmp/x", state = "absent")
      """
    When I apply the bundle
    Then exit code is 0
    When I run "test -L /tmp/link-to-remove" inside the container
    Then exit code is 1
