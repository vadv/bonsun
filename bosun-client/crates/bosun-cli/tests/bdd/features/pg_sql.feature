@docker @pg-sql @todo-skip
Feature: pg_sql.exec / pg_sql.query — нативные PostgreSQL примитивы
  bosun делает CREATE ROLE / GRANT / CREATE EXTENSION / SELECT через
  sync postgres-клиент. Идемпотентность для exec — через опциональный
  `if_not_exists_check`: SELECT возвращает строки → exec пропускается.
  query с `store_as_fact=<name>` публикует результат в runtime-store
  фактов, доступный последующим примитивам.
  Сценарии помечены @todo-skip: требуют docker-compose с реальным
  postgres контейнером рядом с test container. В test-base.Dockerfile
  установлен psql, но сам postgres-сервер отсутствует.

  Scenario: exec создаёт роль, повторный apply — no-change через if_not_exists_check
    Given a fresh container
    And a bundle with manifest:
      """
      pg_sql.exec(
          name = "create-bdd-role",
          dsn = "postgresql://postgres@127.0.0.1/postgres",
          sql = "CREATE ROLE bdd_user LOGIN PASSWORD 'secret'",
          if_not_exists_check = "SELECT 1 FROM pg_roles WHERE rolname = 'bdd_user'",
      )
      """
    When I apply the bundle
    Then exit code is 0
    When I apply the bundle again
    Then exit code is 0
    And stdout contains "no-change"

  Scenario: query store_as_fact публикует результат
    Given a fresh container
    And a bundle with manifest:
      """
      pg_sql.query(
          name = "fetch-version",
          dsn = "postgresql://postgres@127.0.0.1/postgres",
          sql = "SELECT current_setting('server_version_num') AS version",
          store_as_fact = "pg_version",
      )
      file.content(path = "/tmp/pg-version.txt", contents = "version: {{ facts.pg_version[0].version }}")
      """
    When I apply the bundle
    Then exit code is 0
    And file "/tmp/pg-version.txt" contains "version: "
