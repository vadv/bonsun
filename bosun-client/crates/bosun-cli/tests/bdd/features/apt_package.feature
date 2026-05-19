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

  Scenario: Held dpkg lock defers apt.package without failing the run
    # dpkg-lock — транзиентное состояние (unattended-upgrades, ручной apt и
    # т.п.). bosun не должен валить exit-код и флапать метрику failed —
    # ресурс уходит в Deferred, следующий цикл попробует снова.
    #
    # apt/dpkg/unattended-upgrades берут lock через fcntl(F_SETLK, F_WRLCK).
    # BSD-flock(2) и POSIX-fcntl — независимые механизмы, поэтому раньше
    # тест эмулировал blocker через `flock -x`, но bosun-фикс F03 перешёл
    # на fcntl(F_GETLK) probe. Сейчас держатель тоже должен быть fcntl —
    # используем python3, который есть в test-base образе.
    Given a fresh container
    When I run "python3 -u -c 'import fcntl, sys, time; f=open(\"/var/lib/dpkg/lock-frontend\",\"r+\"); fcntl.lockf(f, fcntl.LOCK_EX | fcntl.LOCK_NB); sys.stdout.write(\"locked\\n\"); sys.stdout.flush(); time.sleep(60)' & sleep 1" inside the container
    Then exit code is 0
    Given a bundle with manifest:
      """
      apt.package(name = "curl")
      """
    When I apply the bundle
    Then exit code is 0
    And stdout contains "deferred"

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
