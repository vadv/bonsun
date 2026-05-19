@docker @sysctl-reload
Feature: sysctl.reload primitive
  bosun применяет параметры ядра из конкретного .conf-файла через
  `sysctl -p <path>`. На уровне ядра это идемпотентная операция:
  повторный set того же значения — no-op. Apply падает, если файл,
  на который ссылается ресурс, не существует на момент выполнения.

  @todo-skip
  Scenario: Reload корректно загружает существующий .conf
    # В контейнере без --privileged sysctl -p почти всегда падает на
    # системных параметрах (vm.swappiness, net.*) с EACCES. Полноценное
    # покрытие требует privileged-контейнера; этот сценарий помечен
    # @todo-skip до настройки CI с capabilities.
    Given a fresh container
    And a bundle with manifest:
      """
      file.content(path = "/etc/sysctl.d/60-bosun.conf", contents = "kernel.threads-max = 100000\n")
      sysctl.reload(name = "bosun-kernel", path = "/etc/sysctl.d/60-bosun.conf")
      """
    When I apply the bundle
    Then exit code is 0

  Scenario: Reload падает, если .conf отсутствует
    Given a fresh container
    And a bundle with manifest:
      """
      sysctl.reload(name = "missing-file", path = "/etc/sysctl.d/does-not-exist.conf")
      """
    When I apply the bundle
    Then exit code is 1
