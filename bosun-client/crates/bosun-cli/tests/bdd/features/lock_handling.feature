@docker @lock-handling
Feature: bosun process-level lock
  bosun acquires an exclusive flock on /tmp/bosun.lock so that a second
  invocation cannot run concurrently. When the lock is held, the second
  process exits 0 with an informational message — not failure.

  Scenario: Second bosun while first holds the flock exits 0 with info
    Given a fresh container
    And a bundle with manifest:
      """
      file.content(path = "/etc/locked.conf", contents = "ok")
      """
    When I run "( flock -x 9 ; sleep 30 ) 9>/tmp/bosun.lock & echo $! > /tmp/flock.pid ; sleep 1" inside the container
    Then exit code is 0
    When I apply the bundle
    Then exit code is 0
    And stderr contains "another bosun instance holds"
