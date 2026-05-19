@docker @apt-key
Feature: apt.key primitive — управление signed-by GPG-ключом
  bosun кладёт ключ репозитория в /etc/apt/keyrings/<name>.gpg
  (modern signed-by стиль), а не в legacy /etc/apt/trusted.gpg.
  Источник ключа — либо url, либо inline key_data; legacy apt-key add
  сознательно не поддерживается.

  Scenario: Удаление отсутствующего ключа — no-change
    Given a fresh container
    And a bundle with manifest:
      """
      apt.key(
          name = "absent-key",
          state = "absent",
          keyring_path = "/etc/apt/keyrings/absent-key.gpg",
      )
      """
    When I apply the bundle
    Then exit code is 0
    And stdout contains "no-change"

  Scenario: Удаление существующего keyring-файла
    Given a fresh container
    When I run "mkdir -p /etc/apt/keyrings && echo dummy > /etc/apt/keyrings/to-remove.gpg" inside the container
    Then exit code is 0
    Given a bundle with manifest:
      """
      apt.key(
          name = "to-remove",
          state = "absent",
          keyring_path = "/etc/apt/keyrings/to-remove.gpg",
      )
      """
    When I apply the bundle
    Then exit code is 0
    And file "/etc/apt/keyrings/to-remove.gpg" does not exist in container

  Scenario: Present без url и без key_data — ошибка валидации
    Given a fresh container
    And a bundle with manifest:
      """
      apt.key(
          name = "broken",
          state = "present",
          keyring_path = "/etc/apt/keyrings/broken.gpg",
      )
      """
    When I apply the bundle
    Then exit code is 1
