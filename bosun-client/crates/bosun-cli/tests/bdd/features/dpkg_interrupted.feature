@docker @dpkg-interrupted
Feature: dpkg interrupted state recovery
  When dpkg was killed mid-configure, bosun runs `dpkg --configure -a`
  before retrying install. Producing a reliable half-configured state
  in a fresh container needs a custom postinst that fails; until we
  have a stable fixture, the scenario is marked @todo-skip.

  @todo-skip
  Scenario: bosun runs dpkg --configure -a before install
    Given a fresh container
    And a bundle with manifest:
      """
      apt.package(name = "curl")
      """
    When I apply the bundle
    Then exit code is 0
