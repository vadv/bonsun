@docker @cert-tls
Feature: cert.tls primitive — self-signed x509-сертификат
  bosun генерирует и обновляет self-signed сертификаты pure-Rust пайплайном
  (rcgen + ring, RSA-ключи через rsa-крейт). Имитирует chiit-аналог:
  RSA 2048, 10 лет validity, renew за 30 дней до expiry.

  Scenario: Создание нового сертификата
    Given a fresh container
    And a bundle with manifest:
      """
      cert.tls(
          cert_path = "/etc/ssl/bdd.crt",
          key_path = "/etc/ssl/bdd.key",
          common_name = "bdd.example.com",
      )
      """
    When I apply the bundle
    Then exit code is 0
    And file "/etc/ssl/bdd.crt" exists in container
    And file "/etc/ssl/bdd.key" exists in container
    And certificate at "/etc/ssl/bdd.crt" is valid
    And certificate at "/etc/ssl/bdd.crt" has common name "bdd.example.com"

  Scenario: Приватный ключ имеет mode 600 по умолчанию
    Given a fresh container
    And a bundle with manifest:
      """
      cert.tls(
          cert_path = "/etc/ssl/perm.crt",
          key_path = "/etc/ssl/perm.key",
          common_name = "perm.example.com",
      )
      """
    When I apply the bundle
    Then exit code is 0
    And key at "/etc/ssl/perm.key" has mode 600

  Scenario: Свежий сертификат — повторный apply — no-change
    Given a fresh container
    And a bundle with manifest:
      """
      cert.tls(
          cert_path = "/etc/ssl/idemp.crt",
          key_path = "/etc/ssl/idemp.key",
          common_name = "idemp.example.com",
      )
      """
    When I apply the bundle
    Then exit code is 0
    When I apply the bundle again
    Then exit code is 0
    And stdout contains "no-change"

  Scenario: Ed25519 алгоритм
    Given a fresh container
    And a bundle with manifest:
      """
      cert.tls(
          cert_path = "/etc/ssl/ed.crt",
          key_path = "/etc/ssl/ed.key",
          common_name = "ed.example.com",
          algorithm = "ed25519",
      )
      """
    When I apply the bundle
    Then exit code is 0
    And certificate at "/etc/ssl/ed.crt" is valid
