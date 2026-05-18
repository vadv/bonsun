@docker @file-content
Feature: file.content primitive
  bosun manages plain text files: content, mode, ownership.
  Every scenario starts from a fresh container.

  Scenario: Fresh file is written from manifest
    Given a fresh container
    And a bundle with manifest:
      """
      file.content(path = "/etc/demo.conf", contents = "hello")
      """
    When I apply the bundle
    Then exit code is 0
    And file "/etc/demo.conf" exists in container
    And file "/etc/demo.conf" has content "hello"

  Scenario: Same content and mode produces no change on second run
    Given a fresh container
    And a bundle with manifest:
      """
      file.content(path = "/etc/demo.conf", contents = "hello")
      """
    When I apply the bundle
    Then exit code is 0
    When I apply the bundle again
    Then exit code is 0
    And stdout contains "no-change"

  Scenario: Different content updates the file
    Given a fresh container
    And a bundle with manifest:
      """
      file.content(path = "/etc/demo.conf", contents = "v1")
      """
    When I apply the bundle
    Then exit code is 0
    And file "/etc/demo.conf" has content "v1"
    Given a bundle with manifest:
      """
      file.content(path = "/etc/demo.conf", contents = "v2")
      """
    When I apply the bundle
    Then exit code is 0
    And file "/etc/demo.conf" has content "v2"

  Scenario: Custom mode is applied
    Given a fresh container
    And a bundle with manifest:
      """
      file.content(path = "/etc/secret.conf", contents = "x", mode = 0o600)
      """
    When I apply the bundle
    Then exit code is 0
    And file "/etc/secret.conf" exists in container
    And file "/etc/secret.conf" has mode 600

  Scenario: Owner root is applied when running as root
    Given a fresh container
    And a bundle with manifest:
      """
      file.content(path = "/etc/owned.conf", contents = "x", owner = "root", group = "root")
      """
    When I apply the bundle
    Then exit code is 0
    And file "/etc/owned.conf" has owner "root"

  Scenario: Symlink at target path is rejected
    Given a fresh container
    When I run "ln -s /etc/passwd /etc/symlinked.conf" inside the container
    Then exit code is 0
    Given a bundle with manifest:
      """
      file.content(path = "/etc/symlinked.conf", contents = "wont write")
      """
    When I apply the bundle
    Then exit code is 1

  @slow
  Scenario: Seven sequential updates keep exactly five rotated backups
    Given a fresh container
    And a bundle with manifest:
      """
      file.content(path = "/etc/rotated.conf", contents = "v1")
      """
    When I apply the bundle
    Then exit code is 0
    Given a bundle with manifest:
      """
      file.content(path = "/etc/rotated.conf", contents = "v2")
      """
    When I run "sleep 1" inside the container
    When I apply the bundle
    Given a bundle with manifest:
      """
      file.content(path = "/etc/rotated.conf", contents = "v3")
      """
    When I run "sleep 1" inside the container
    When I apply the bundle
    Given a bundle with manifest:
      """
      file.content(path = "/etc/rotated.conf", contents = "v4")
      """
    When I run "sleep 1" inside the container
    When I apply the bundle
    Given a bundle with manifest:
      """
      file.content(path = "/etc/rotated.conf", contents = "v5")
      """
    When I run "sleep 1" inside the container
    When I apply the bundle
    Given a bundle with manifest:
      """
      file.content(path = "/etc/rotated.conf", contents = "v6")
      """
    When I run "sleep 1" inside the container
    When I apply the bundle
    Given a bundle with manifest:
      """
      file.content(path = "/etc/rotated.conf", contents = "v7")
      """
    When I run "sleep 1" inside the container
    When I apply the bundle
    Then exit code is 0
    And there are 5 backup files in "/tmp/bosun-backups/etc"
