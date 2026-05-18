@docker @template
Feature: template rendering
  bosun renders Jinja-style templates from bundle/templates/ with
  inventory and facts available as `inv` and `inv.facts`.

  Scenario: Basic render with inventory variable
    Given a fresh container
    And a bundle with inventory:
      """
      app:
        name: nginx
      """
    And the bundle has a template "greeting.j2" with content:
      """
      app: {{ inv.app.name }}
      """
    And a bundle with manifest:
      """
      content = template("greeting.j2")
      file.content(path = "/etc/greeting.conf", contents = content)
      """
    When I apply the bundle
    Then exit code is 0
    And file "/etc/greeting.conf" contains "app: nginx"

  Scenario: Render with facts.hostname
    Given a fresh container
    And the bundle has a template "host.j2" with content:
      """
      host={{ inv.facts.hostname }}
      """
    And a bundle with manifest:
      """
      content = template("host.j2")
      file.content(path = "/etc/host.conf", contents = content)
      """
    When I apply the bundle
    Then exit code is 0
    And file "/etc/host.conf" contains "host="

  Scenario: Missing inventory key fails with strict error
    Given a fresh container
    And an empty inventory
    And the bundle has a template "missing.j2" with content:
      """
      val={{ inv.missing_key }}
      """
    And a bundle with manifest:
      """
      content = template("missing.j2")
      file.content(path = "/etc/missing.conf", contents = content)
      """
    When I apply the bundle
    Then exit code is 3
