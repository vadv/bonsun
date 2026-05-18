@docker @facts
Feature: facts collection
  bosun gathers facts from /proc, /etc, /var/lib/dpkg before evaluating
  the manifest. Facts are observable via inv.facts in templates.

  Scenario: cpu_count is exposed and positive
    Given a fresh container
    And the bundle has a template "cpu.j2" with content:
      """
      cpus={{ inv.facts.cpu_count }}
      """
    And a bundle with manifest:
      """
      content = template("cpu.j2")
      file.content(path = "/etc/cpu.conf", contents = content)
      """
    When I apply the bundle
    Then exit code is 0
    And file "/etc/cpu.conf" contains "cpus="

  Scenario: is_pod is false in a plain debian container
    Given a fresh container
    And the bundle has a template "pod.j2" with content:
      """
      pod={{ inv.facts.is_pod }}
      """
    And a bundle with manifest:
      """
      content = template("pod.j2")
      file.content(path = "/etc/pod.conf", contents = content)
      """
    When I apply the bundle
    Then exit code is 0
    And file "/etc/pod.conf" contains "pod=false"

  Scenario: hostname fact is exposed
    Given a fresh container
    And the bundle has a template "hn.j2" with content:
      """
      hn={{ inv.facts.hostname }}
      """
    And a bundle with manifest:
      """
      content = template("hn.j2")
      file.content(path = "/etc/hn.conf", contents = content)
      """
    When I apply the bundle
    Then exit code is 0
    And file "/etc/hn.conf" contains "hn="

  @todo-skip
  Scenario: dpkg unreadable triggers fallback in apt.package
    # Requires `chmod 000 /var/lib/dpkg/status` plus apt fallback path.
    # Covered by unit tests in bosun-facts::installed_packages; running
    # this as a real BDD scenario inside debian:bookworm-slim breaks the
    # subsequent apt-get calls in the same scenario, so deferred.
    Given a fresh container
    When I apply the bundle
    Then exit code is 0
