@docker @apt-package
Feature: apt.package primitive
  bosun installs Debian packages via apt-get, handles dpkg lock and
  half-configured state. Container is debian:bookworm-slim by default.

  @slow
  Scenario: Fresh install of curl
    Given a fresh container
    And a bundle with manifest:
      """
      apt.package(name = "curl")
      """
    When I apply the bundle
    Then exit code is 0
    And package "curl" is installed

  @slow
  Scenario: Already installed package is no-change
    Given a fresh container
    When I run "apt-get install -y --no-install-recommends curl" inside the container
    Then exit code is 0
    Given a bundle with manifest:
      """
      apt.package(name = "curl")
      """
    When I apply the bundle
    Then exit code is 0
    And stdout contains "no-change"

  @slow
  Scenario: Idempotent re-apply makes no change
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

  Scenario: Held dpkg lock blocks apply with exit 1
    Given a fresh container
    When I run "( flock -x 9 ; sleep 60 ) 9>/var/lib/dpkg/lock-frontend & sleep 1" inside the container
    Then exit code is 0
    Given a bundle with manifest:
      """
      apt.package(name = "curl")
      """
    When I apply the bundle
    Then exit code is 1

  @todo-skip @dpkg-interrupted
  Scenario: Half-configured dpkg state triggers `dpkg --configure -a` recovery
    # Synthesizing a reliable half-configured dpkg state inside a fresh
    # container is fragile (requires interrupting a postinst at the right
    # microsecond). Recovery itself is unit-tested in apt_package::recovery;
    # the BDD coverage is deferred until we have a stable fixture.
    Given a fresh container
    And a bundle with manifest:
      """
      apt.package(name = "curl")
      """
    When I apply the bundle
    Then exit code is 0

  @todo-skip @slow
  Scenario: Transient 503 on apt-get update is retried
    # Requires a mock apt-mirror that returns 503 the first two attempts.
    # Spinning that up alongside the test container is out-of-scope for
    # Phase 9 MVP; the retry loop is unit-tested in apt_package::exec.
    Given a fresh container
    And a bundle with manifest:
      """
      apt.package(name = "curl")
      """
    When I apply the bundle
    Then exit code is 0
