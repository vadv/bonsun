@docker @validate-with
Feature: validate_with — атомарная замена с предварительной валидацией
  file.content c validate_with рендерит `<path>.new`, запускает validator
  на этом файле и только при exit=0 swap'ает в `<path>`. На failure
  `<path>.new` остаётся для forensics, target не трогается. Это chiit-
  паттерн «nginx -t / pgbouncer -t перед applyem конфигурации».

  Scenario: Validator exit 0 — swap происходит
    Given a fresh container
    And a bundle with manifest:
      """
      file.content(
          path = "/etc/validated.conf",
          contents = "good config\n",
          validate_with = ["/bin/true"],
      )
      """
    When I apply the bundle
    Then exit code is 0
    And file "/etc/validated.conf" exists in container
    And file "/etc/validated.conf" contains "good config"

  Scenario: Validator exit 1 — swap не происходит, target отсутствует
    Given a fresh container
    And a bundle with manifest:
      """
      file.content(
          path = "/etc/blocked.conf",
          contents = "bad config\n",
          validate_with = ["/bin/false"],
      )
      """
    When I apply the bundle
    Then exit code is 1
    And file "/etc/blocked.conf" does not exist in container

  Scenario: Validator exit 1 — .new остаётся на диске для forensics
    Given a fresh container
    And a bundle with manifest:
      """
      file.content(
          path = "/etc/forensics.conf",
          contents = "this is what failed validation\n",
          validate_with = ["/bin/false"],
      )
      """
    When I apply the bundle
    Then exit code is 1
    And file "/etc/forensics.conf.new" exists in container
    And file "/etc/forensics.conf.new" contains "this is what failed validation"

  Scenario: Validator с placeholder {new_path}
    Given a fresh container
    And a bundle with manifest:
      """
      file.content(
          path = "/etc/checked.conf",
          contents = "keyword token\n",
          validate_with = ["/bin/grep", "-q", "keyword", "{new_path}"],
      )
      """
    When I apply the bundle
    Then exit code is 0
    And file "/etc/checked.conf" contains "keyword token"

  Scenario: Validator падает — повторный apply без изменений тоже падает
    Given a fresh container
    And a bundle with manifest:
      """
      file.content(
          path = "/etc/idem-fail.conf",
          contents = "x\n",
          validate_with = ["/bin/false"],
      )
      """
    When I apply the bundle
    Then exit code is 1
    When I apply the bundle again
    Then exit code is 1
    And file "/etc/idem-fail.conf" does not exist in container

  Scenario: Validator с существующим target — на failure target не теряется
    Given a fresh container
    When I run "echo 'original' > /etc/preserved.conf" inside the container
    Then exit code is 0
    Given a bundle with manifest:
      """
      file.content(
          path = "/etc/preserved.conf",
          contents = "new content that fails validation\n",
          validate_with = ["/bin/false"],
      )
      """
    When I apply the bundle
    Then exit code is 1
    And file "/etc/preserved.conf" contains "original"
    And file "/etc/preserved.conf" does not contain "new content"
