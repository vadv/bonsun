@docker @idempotency
Feature: idempotent re-runs
  Re-running bosun apply with the same bundle must not perform any
  changes after the first successful run.

  Scenario: file.content re-applied is no-change
    Given a fresh container
    And a bundle with manifest:
      """
      file.content(path = "/etc/idemp.conf", contents = "x")
      """
    When I apply the bundle
    Then exit code is 0
    When I apply the bundle again
    Then exit code is 0
    And stdout contains "no-change"

  Scenario: template + file.content re-applied is no-change
    Given a fresh container
    And a bundle with inventory:
      """
      app:
        name: nginx
      """
    And the bundle has a template "g.j2" with content:
      """
      app: {{ inv.app.name }}
      """
    And a bundle with manifest:
      """
      content = template("g.j2")
      file.content(path = "/etc/g.conf", contents = content)
      """
    When I apply the bundle
    Then exit code is 0
    When I apply the bundle again
    Then exit code is 0
    And stdout contains "no-change"

  @slow
  Scenario: apt.package re-applied is no-change
    Given a fresh container
    And a bundle with manifest:
      """
      apt.package(name = "curl")
      """
    When I apply the bundle
    Then exit code is 0
    When I apply the bundle again
    Then exit code is 0
    And stdout contains "no-change"
