@docker @users
Feature: users.user / users.group primitives
  bosun декларативно управляет системными пользователями и группами через
  стандартные утилиты useradd/groupadd/userdel/groupdel. Семантика — узкая:
  если пользователь уже есть с нужными полями, никаких usermod не вызываем.

  Scenario: Создание группы Present
    Given a fresh container
    And a bundle with manifest:
      """
      users.group(name = "bosun-group", state = "present", system = True)
      """
    When I apply the bundle
    Then exit code is 0
    And group "bosun-group" exists in container

  Scenario: Создание группы с фиксированным gid
    Given a fresh container
    And a bundle with manifest:
      """
      users.group(name = "fixed-gid", state = "present", gid = 5432, system = True)
      """
    When I apply the bundle
    Then exit code is 0
    And group "fixed-gid" has gid 5432

  Scenario: Повторное создание группы — no-change
    Given a fresh container
    And a bundle with manifest:
      """
      users.group(name = "twice", state = "present", system = True)
      """
    When I apply the bundle
    Then exit code is 0
    When I apply the bundle again
    Then exit code is 0
    And stdout contains "no-change"

  Scenario: Удаление существующей группы
    Given a fresh container
    When I run "groupadd transient" inside the container
    Then exit code is 0
    Given a bundle with manifest:
      """
      users.group(name = "transient", state = "absent")
      """
    When I apply the bundle
    Then exit code is 0
    And group "transient" does not exist in container

  Scenario: Создание пользователя с явным shell /bin/false
    Given a fresh container
    And a bundle with manifest:
      """
      users.user(name = "bosun-user", state = "present", system = True, no_create_home = True, shell = "/bin/false")
      """
    When I apply the bundle
    Then exit code is 0
    And user "bosun-user" exists in container
    And user "bosun-user" has shell "/bin/false"

  Scenario: Создание пользователя с фиксированным uid
    Given a fresh container
    And a bundle with manifest:
      """
      users.user(name = "fixed-uid", state = "present", uid = 5433, system = True, no_create_home = True)
      """
    When I apply the bundle
    Then exit code is 0
    And user "fixed-uid" has uid 5433

  Scenario: Повторное создание пользователя — no-change
    Given a fresh container
    And a bundle with manifest:
      """
      users.user(name = "twice-user", state = "present", system = True, no_create_home = True)
      """
    When I apply the bundle
    Then exit code is 0
    When I apply the bundle again
    Then exit code is 0
    And stdout contains "no-change"

  Scenario: Удаление пользователя state=absent
    Given a fresh container
    When I run "useradd -r -M -s /bin/false ghost" inside the container
    Then exit code is 0
    Given a bundle with manifest:
      """
      users.user(name = "ghost", state = "absent")
      """
    When I apply the bundle
    Then exit code is 0
    And user "ghost" does not exist in container

  Scenario: Обновление shell через apply
    Given a fresh container
    When I run "useradd -r -M -s /bin/false changesh" inside the container
    Then exit code is 0
    Given a bundle with manifest:
      """
      users.user(name = "changesh", state = "present", shell = "/usr/sbin/nologin", no_create_home = True)
      """
    When I apply the bundle
    Then exit code is 0
    And user "changesh" has shell "/usr/sbin/nologin"
