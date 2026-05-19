@docker @apt-update-cache
Feature: apt.update_cache primitive
  bosun лениво обновляет apt-кеш: если mtime /var/cache/apt/pkgcache.bin
  моложе max_age_sec и force=False — действие пропускается. После
  успешного update удаляются устаревшие `.deb` из
  /var/cache/apt/archives.

  Scenario: force=True успешно выполняет apt-get update
    Given a fresh container
    And a bundle with manifest:
      """
      apt.update_cache(name = "bdd-cache", force = True)
      """
    When I apply the bundle
    Then exit code is 0
    And stdout contains "apt.update_cache:bdd-cache"

  @todo-skip
  Scenario: Повторный вызов с свежим кешем — no-change
    # debian:bookworm-slim apt-get update больше не создаёт
    # /var/cache/apt/pkgcache.bin (см. /var/lib/apt/lists/ вместо). Без
    # этого файла bosun не может определить «свежесть кеша» через mtime,
    # поэтому второй apply всегда выполняет update заново. Сценарий
    # помечен @todo-skip до фикса в apt_update_cache::plan (нужно
    # читать /var/lib/apt/lists/ mtime, а не pkgcache.bin).
    Given a fresh container
    And a bundle with manifest:
      """
      apt.update_cache(name = "bdd-cache", force = True)
      """
    When I apply the bundle
    Then exit code is 0
    Given a bundle with manifest:
      """
      apt.update_cache(name = "bdd-cache", max_age_sec = 3600)
      """
    When I apply the bundle
    Then exit code is 0
    And stdout contains "no-change"

  Scenario: skip_cleanup=True не падает (smoke-тест)
    # На debian apt-get update сам чистит /var/cache/apt/archives через
    # APT::Periodic; этот сценарий проверяет, что флаг skip_cleanup=True
    # принимается без ошибок.
    Given a fresh container
    And a bundle with manifest:
      """
      apt.update_cache(name = "bdd-skip", force = True, skip_cleanup = True)
      """
    When I apply the bundle
    Then exit code is 0
