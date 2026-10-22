# Equestrian Dream Launcher

Версия: **0.1.13**

Настольный Minecraft-лаунчер проекта EquestrianDream для Windows, написанный на Rust и Slint.

## Разработка

Release-сборка:

```powershell
cargo build --release
```

Исполняемый файл: `target/release/EquestrianDreamLauncher.exe`. Java в EXE не включена; при необходимости лаунчер скачивает MSI с хоста проекта. Настройка `java21_msi_url` и проверки файла описаны в [docs/JAVA_INSTALLATION.md](docs/JAVA_INSTALLATION.md).
Архитектура, настройка backend и команды проверок описаны в [AGENTS.md](AGENTS.md).
Инструкция для пользователей: [packaging/README.txt](packaging/README.txt).

## Версии и выпуск

Единственный источник версии — `[workspace.package].version` в `Cargo.toml`.
Все пакеты workspace наследуют её; приложение и Windows EXE получают номер через Cargo.
Номер повышается один раз при подготовке выпуска после внесения изменений.

| Изменение | Команда | Пример |
| --- | --- | --- |
| Исправления | `.\scripts\release.ps1 patch` | `0.1.3 → 0.1.4` |
| Новые возможности | `.\scripts\release.ps1 minor` | `0.1.3 → 0.2.0` |
| Несовместимые изменения | `.\scripts\release.ps1 major` | `0.1.3 → 1.0.0` |

Запускайте из PowerShell, например:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\release.ps1 patch
```

Скрипт синхронно обновляет `Cargo.toml`, версии пакетов workspace в `Cargo.lock`,
строку «Версия» в этом README и заголовок `packaging/README.txt`.
Версии сторонних зависимостей и остальной текст сохраняются.
`minor` обнуляет patch; `major` обнуляет minor и patch.
Поддерживаются стабильные версии `MAJOR.MINOR.PATCH`, каждый компонент — до 65535
(ограничение Windows EXE). При ошибке записи исходные файлы восстанавливаются.

Предварительный просмотр и отдельная проверка:

```powershell
.\scripts\release.ps1 patch -WhatIf
.\scripts\release.ps1 -Check
```

После проверки изменений подготовьте дистрибутив:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\package-release.ps1
```

Упаковка сначала проверяет согласованность версий, затем выполняет проверки Cargo
и собирает EXE и ZIP. Повторная упаковка сохраняет текущую версию.
Скрипты не создают коммиты или теги и не публикуют обновления.
Порядок публикации: [docs/LAUNCHER_UPDATES.md](docs/LAUNCHER_UPDATES.md).
