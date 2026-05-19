@docker @health-check
Feature: health_check примитива service.unit — cmd-вариант
  service.unit умеет проверять, что unit реально живой через cmd-probe
  (`/usr/sbin/nginx -t`, `pg_isready -h ...`). Retry-цикл с
  cancellation; на превышении retry_count primitive падает с
  HealthCheckError, и notify-driven restart уходит в defer.
  Сценарии используют bundle validate + facts fixture, чтобы проверить
  что health_check_* kwargs принимаются service.unit'ом, не требуя
  настоящего runr/systemd.

  Scenario: health_check_cmd принимается service.unit'ом
    Given a fresh container
    And facts fixture init_system = "systemd"
    And a bundle with manifest:
      """
      service.unit(
          name = "nginx",
          state = "running",
          health_check_cmd = ["/bin/true"],
          health_check_retry = 1,
          health_check_timeout_sec = 5,
      )
      """
    When I validate the bundle
    Then exit code is 0

  Scenario: health_check_url принимается service.unit'ом
    Given a fresh container
    And facts fixture init_system = "runr"
    And a bundle with manifest:
      """
      service.unit(
          name = "nginx",
          state = "running",
          health_check_url = "http://127.0.0.1:8080/healthz",
          health_check_expected_status = 200,
          health_check_retry = 3,
          health_check_retry_interval_sec = 1,
      )
      """
    When I validate the bundle
    Then exit code is 0

  Scenario: validate_with в service.unit принимается
    Given a fresh container
    And facts fixture init_system = "systemd"
    And a bundle with manifest:
      """
      service.unit(
          name = "nginx",
          state = "running",
          validate_with = ["/usr/sbin/nginx", "-t"],
      )
      """
    When I validate the bundle
    Then exit code is 0
