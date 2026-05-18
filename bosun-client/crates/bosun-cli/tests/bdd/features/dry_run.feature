@docker @dry-run
Feature: bosun apply --dry-run
  bosun reports the plan without changing the system. Exit code is 0 if
  no drift is detected, 2 if drift exists.

  Scenario: Dry-run on already-converged file reports no drift
    Given a fresh container
    And a bundle with manifest:
      """
      file.content(path = "/etc/drift.conf", contents = "ok")
      """
    When I apply the bundle
    Then exit code is 0
    When I apply the bundle in dry-run mode
    Then exit code is 0
    And stdout contains "no-change"

  Scenario: Dry-run on missing file reports drift with exit 2
    Given a fresh container
    And a bundle with manifest:
      """
      file.content(path = "/etc/missing-drift.conf", contents = "ok")
      """
    When I apply the bundle in dry-run mode
    Then exit code is 2
    And file "/etc/missing-drift.conf" does not exist in container
