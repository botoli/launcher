# AGENTS.md — Equestrian Dream Launcher

## Назначение документа

Описание текущего состояния репозитория и руководство для разработчиков и кодовых агентов. Подготовлено по исходному коду и конфигурации 14 октября 2026 года. При изменении архитектуры, команд, API или путей хранения обновляйте этот файл.

До появления этого документа в проекте была только пользовательская инструкция `packaging/README.txt`: запуск готовой Windows-сборки, Java, данные и backend. Полного описания разработки и архитектуры не было.

## Что это за проект

Equestrian Dream Launcher — настольный лаунчер Minecraft для проекта EquestrianDream. Он получает параметры сборки с внешнего backend, синхронизирует моды, подготавливает Minecraft Vanilla или NeoForge, находит установленную Java и запускает игровой процесс с указанным ником и адресом сервера.

Основная целевая платформа поставки — Windows. Дистрибутив — папка с `EquestrianDreamLauncher.exe` и `README.txt`, без установщика и цифровой подписи. В коде есть платформенные условные ветви, но готовность Linux/macOS этим репозиторием и этим документом не подтверждается.

Backend, сервер Minecraft, исходники модов и готовая игровая сборка в исходную структуру проекта не входят. Их адреса и содержимое задаются внешними сервисами. Наличие URL в коде не подтверждает доступность сервиса.

## Технологии

- Rust, Cargo workspace, edition `2024`, resolver `3`.
- Версия workspace: `0.1.0`; поле license: `MIT OR Apache-2.0`.
- Slint `1.17.1` в manifest для интерфейса и компиляции `.slint`; точные разрешённые версии зависимостей фиксирует `Cargo.lock`.
- Tokio для асинхронной работы, каналов, файлов, сети и процессов.
- Reqwest `0.12` с `rustls-tls`, без default features; Serde/JSON для конфигурации и метаданных.
- SHA-1/SHA-256 для проверки загружаемых артефактов; ZIP для модов, natives и NeoForge.
- `tracing`, `tracing-subscriber`, `tracing-appender`, `rolling-file` для логирования.
- `windows-sys` и `winresource` для интеграции Windows; PowerShell для упаковки.

## Структура репозитория

| Путь | Ответственность |
| --- | --- |
| `Cargo.toml`, `Cargo.lock` | Состав workspace, общая версия, зависимости и их фиксация |
| `apps/desktop/` | Пакет `launcher-desktop`, исполняемый файл `EquestrianDreamLauncher` |
| `apps/desktop/src/main.rs` | Инициализация приложения, привязка UI к ядру, команды, отображение состояний |
| `apps/desktop/src/app_config.rs` | Backend, переменные окружения и каталоги данных |
| `apps/desktop/src/settings.rs` | Сохранение ника и работа с offline-профилем |
| `apps/desktop/src/logging.rs` | Файловые логи и ротация |
| `apps/desktop/src/platform.rs` | Защита от второго экземпляра и системные сообщения |
| `apps/desktop/build.rs` | Компиляция Slint, встраивание ресурсов, генерация Windows-иконки и version info |
| `apps/desktop/ui/main.slint` | Главное окно |
| `apps/desktop/ui/components/` | Заголовок, основная панель, карточки, кнопки, прогресс |
| `apps/desktop/ui/theme/` | Палитра и каталог графических ресурсов |
| `crates/launcher-core/` | Библиотека логики лаунчера, независимая от Slint |
| `crates/shared/` | Пакет `launcher-shared`: заготовка общих типов, пока только комментарий о назначении |
| `assets/` | Встроенная графика, декорации, иконки и исходные изображения |
| `scripts/package-release.ps1` | Проверки, release-сборка и подготовка дистрибутива |
| `packaging/README.txt` | Инструкция пользователю, включаемая в дистрибутив |
| `target/` | Результаты Cargo, генерируемый каталог |
| `dist/` | Результат упаковки, генерируемый каталог |
| `.launcher-data/` | Локальные данные; каталог исключён из Git, но не является путём хранения по умолчанию |

`.gitignore` исключает `/target/`, `/.launcher-data/` и `/dist/`.

## Архитектура ядра

Публичная граница библиотеки находится в `crates/launcher-core/src/lib.rs`.

- `LauncherCore` принимает `LauncherCommand::Play` и `Cancel` через Tokio `mpsc`.
- Снимки `LauncherState` передаются подписчикам через `watch`; offline-профиль также передаётся через `watch`.
- Состояния: `Idle`, `Checking`, `Downloading`, `Ready`, `Launching`, `Running`, `AuthenticationRequired`, `ProfileRequired`, `Error`.
- Повторный `Play` во время текущей операции не должен создавать параллельную установку/запуск.
- `game_actor.rs` связывает подготовку Vanilla/NeoForge, прогресс, отмену и наблюдение за игровым процессом.
- Desktop подписывается на состояние и обновляет Slint через механизм event loop. Сетевые и игровые операции принадлежат ядру.

| Модули внутри `crates/launcher-core/src/` | Назначение |
| --- | --- |
| `client_config.rs` | Backend URL, получение и разбор `/api/client-config` |
| `mods.rs` | Загрузка ZIP модов, проверка путей, staging, замена каталога и локальное состояние |
| `manifest.rs`, `verification.rs` | Файловый manifest: версия, версия Minecraft, пути, SHA-256 и размеры; проверка установки |
| `source.rs`, `http.rs`, `installer.rs` | Локальные/HTTP-источники, загрузка manifest, установка файлов и отмена |
| `identity.rs` | Offline-профиль, UUID и идентичность для запуска |
| `auth/` | Microsoft device-code flow, обновление токена, Xbox/XSTS, Minecraft Services, entitlement, профиль и сессия |
| `java.rs` | Поиск Java, вызов `java -version`, выбор требуемой major-версии |
| `process.rs`, `process_command.rs` | Команда запуска Java, аргументы, stdout/stderr, завершение и отмена процесса |
| `minecraft/version_catalog.rs`, `metadata.rs`, `rules.rs` | Каталог версий Mojang, метаданные, выбор библиотек и платформенные правила |
| `minecraft/acquisition.rs`, `download.rs`, `assets.rs` | Получение клиента, библиотек и assets с проверкой артефактов |
| `minecraft/natives.rs`, `runtime.rs` | Распаковка native-библиотек, размещение и проверка runtime |
| `minecraft/launch.rs`, `launcher.rs` | Подстановка аргументов, подготовка и запуск Vanilla |
| `minecraft/neoforge/` | Метаданные, installer, processors, библиотеки и запуск NeoForge |

В библиотеке есть как установка по `PackManifest` из локального/удалённого источника, так и backend-сценарий с ZIP модов. Desktop использует backend-сценарий через `LauncherCore::new_backend_with_game`.

## Сценарий работы приложения

1. Читается конфигурация окружения, создаются каталоги, включаются логи и защита от второго экземпляра Windows.
2. Создаются Tokio runtime и Slint-окно. Загружается сохранённый ник из `state/settings.json`.
3. До запуска оконного event loop запрашивается начальная конфигурация backend. Версия Minecraft и адрес сервера попадают в UI. При ошибке конфигурации отображается ошибка и запуск игры блокируется.
4. Пользователь вводит ник и нажимает «Играть». Ник проверяется и сохраняется; ядро получает offline-профиль и команду `Play`.
5. Ядро повторно получает client config, затем синхронизирует моды по `modsHash` и `modsURL`.
6. Выбирается loader `vanilla` или `neoforge` без учёта регистра. Неизвестный loader приводит к `UnsupportedLoader`.
7. Подготавливаются Minecraft, библиотеки, assets, natives и при необходимости NeoForge. Java ищется локально.
8. Запускается процесс Minecraft. Ядро публикует прогресс и состояние `Running`, отслеживает завершение, после успешного сценария возвращается в `Ready`.

Отмена реализована на уровне ядра и отдельных операций. Отдельной кнопки отмены в текущем главном окне нет.

## Backend и контракт конфигурации

Backend по умолчанию: `http://89.125.214.122:3000` (`PRODUCTION_BACKEND_URL` в `app_config.rs`). Endpoint строится как абсолютный путь `/api/client-config` относительно origin: префикс пути в базовом URL не сохраняется.

`ClientConfig` ожидает следующие поля JSON:

| Поле | Тип и смысл |
| --- | --- |
| `version` | Строка, версия Minecraft |
| `loader` | Строка, поддерживаются `vanilla` и `neoforge` |
| `loaderVersion` | Строка, версия загрузчика |
| `javaVersion` | Целое без знака, требуемая major-версия Java |
| `minMemory`, `maxMemory` | Целые без знака, параметры памяти для NeoForge JVM, в МБ |
| `jvmArgs` | Массив строк дополнительных JVM-аргументов для NeoForge |
| `servers` | Массив объектов с полями `name` и `address` |
| `modsHash` | Строка-маркер версии набора модов |
| `modsURL` | Абсолютный HTTP(S) URL ZIP-архива модов |
| `serverIP` | Строка, предпочтительный адрес игрового сервера |

Поля обязательны при десериализации текущей структуры. Адрес сервера выбирается из непустого `serverIP`, иначе из первого непустого `servers[].address`. Для Vanilla `for_client_config` переносит версию, Java и адрес сервера; передача `minMemory`, `maxMemory`, `jvmArgs` из backend реализована в конфигурации NeoForge.

Ограничение ответа client config — 1 МиБ. Принимаются HTTP и HTTPS; URL проверяются библиотечным валидатором. Backend по умолчанию использует HTTP.

Внешние источники runtime:

- Mojang version catalog: `https://piston-meta.mojang.com/mc/game/version_manifest_v2.json`.
- Minecraft assets: `https://resources.download.minecraft.net/`.
- NeoForge Maven: `https://maven.neoforged.net/releases/`.
- Остальные URL артефактов берутся из метаданных соответствующего источника.

## Моды и целостность файлов

`sync_mods` сравнивает `modsHash` с `game/state.json`. При совпадении скачивание пропускается. Это сравнение маркера версии: оно не пересчитывает хеш ZIP или каждого установленного мода.

При изменении маркера ZIP скачивается во временный файл, распаковывается в staging и заменяет каталог `game/mods`; состояние сохраняется вместе с обновлением. Код предусматривает backup/rollback при ошибках фиксации. Архив поддерживает файлы в корне либо обёртку `mods/`; смешанная структура отвергается.

Лимиты: ZIP — 1 ГиБ, распакованные данные — 2 ГиБ, до 10 000 записей. Проверяются небезопасные пути, типы записей и коллизии имён. Эти проверки и восстановление при отмене/ошибках должны сохраняться при доработке.

Отдельный manifest-сценарий использует `PackManifest { version, minecraft_version, files }`, где файл содержит `path`, `sha256`, `size`. Не путайте его с backend-контрактом ZIP модов. Загрузки Minecraft используют контрольные суммы из метаданных.

## Конфигурация и данные

| Переменная | Поведение |
| --- | --- |
| `LAUNCHER_SERVER_URL` | Переопределяет backend; если отсутствует, используется встроенный production URL |
| `LAUNCHER_DATA_DIR` | Переопределяет корневую папку данных |
| `LOCALAPPDATA` | Без `LAUNCHER_DATA_DIR` корень равен `%LOCALAPPDATA%\EquestrianDreamLauncher`; если `LOCALAPPDATA` отсутствует, конфигурация завершается ошибкой |
| `JAVA_HOME`, `PATH` | Источники поиска локальной Java; также проверяются стандартные Windows-каталоги поставщиков и `%USERPROFILE%\.jdks` |

Пустые значения `LAUNCHER_SERVER_URL` и `LAUNCHER_DATA_DIR` считаются ошибкой. На обычной Windows обязательных пользовательских переменных нет.

```text
%LOCALAPPDATA%\EquestrianDreamLauncher\
  game\
    versions\       # JSON/JAR версий, natives и runtime-файлы
    libraries\      # Java-библиотеки
    assets\         # Minecraft assets
    mods\           # Управляемый набор модов
    state.json      # Сохранённый modsHash
  state\
    settings.json   # { "username": "Player" }
  logs\
    launcher.log
```

При `LAUNCHER_DATA_DIR` та же структура создаётся в указанном каталоге. Ник сохраняется атомарно. Некорректные настройки профиля игнорируются с предупреждением в логе. Лог ротируется при 5 МиБ, сохраняются 3 архива; debug-сборка дополнительно пишет в stderr.

## Профиль и ограничения текущей реализации

- Desktop использует offline-профиль: ник длиной 3–16 символов, латинские буквы, цифры и `_`; UUID вычисляется из `OfflinePlayer:<ник>`. Это не автономный режим загрузки: backend и источники runtime всё равно нужны.
- Microsoft/Xbox/Minecraft-авторизация реализована как API библиотеки, но не подключена к текущему пользовательскому сценарию Desktop. Сохранение токенов и окно входа не следует считать реализованными.
- Кнопки «Настройки» и «Моды» вызывают только debug-лог; соответствующих экранов пока нет.
- `server_online = true` и `ping_ms = 43` заданы в `DesktopDisplayState::default`; реальной проверки доступности и измерения пинга нет.
- Окно без стандартной рамки, размер фиксирован 1180×750; UI на русском, шрифт по умолчанию Segoe UI, графика встроена в executable.
- Автоматическая установка Java отсутствует. Инструкция поставки требует Java 21 x64 для текущей сборки; фактическая major-версия при запуске определяется конфигурацией. Поиск выбирает точное совпадение major, а не любую более новую Java.
- Fabric, Forge и Quilt в выборе backend loader не поддерживаются; поддержка NeoForge не означает поддержку Forge.

## Сборка и запуск для разработки

Нужны Rust/Cargo с поддержкой edition 2024 и используемых зависимостей, компоненты `rustfmt`/`clippy`. Для Windows-сборки нужен соответствующий native toolchain, linker и Windows SDK/resource tools (обычно Visual Studio Build Tools с C++). Фиксированного `rust-toolchain.toml` в репозитории нет; точная минимальная версия Rust не заявлена.

Из корня репозитория:

```powershell
cargo check --workspace --all-targets
cargo run -p launcher-desktop --bin EquestrianDreamLauncher
cargo build -p launcher-desktop --release
```

Release executable: `target/release/EquestrianDreamLauncher.exe`. Для самого приложения Rust на машине пользователя не требуется; Java устанавливается отдельно. При первой сборке Cargo нужны зависимости, при подготовке игры — сетевые источники runtime.

Для изолированных данных при разработке можно задать:

```powershell
$env:LAUNCHER_DATA_DIR = Join-Path (Get-Location) '.launcher-data'
# Только если используется собственный запущенный backend:
# $env:LAUNCHER_SERVER_URL = 'http://127.0.0.1:3000'
cargo run -p launcher-desktop --bin EquestrianDreamLauncher
```

Команд для запуска backend в этом репозитории нет.

## Проверки и выпуск

Проверки из существующего release-скрипта:

```powershell
cargo fmt --all -- --check
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
```

Тесты расположены в `#[cfg(test)]` модулях рядом с реализацией. Есть JSON fixtures старых/современных метаданных в `minecraft/fixtures/`, временные каталоги, локальный тестовый HTTP-сервер, синтетические launchers/process helpers. Покрываются конфигурация, профили, установка/проверка, отмена, ZIP-пути, runtime, авторизация и сценарии backend/NeoForge. Эти тесты не заменяют проверку настоящего игрового запуска с внешними сервисами.

Упаковка для Windows:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\package-release.ps1
```

Скрипт выполняет перечисленные проверки, собирает release с `RUSTFLAGS` дополненным `-C target-feature=+crt-static`, затем формирует `dist/EquestrianDreamLauncher/` с EXE и README. Существующая выходная папка пакета заменяется. ZIP, installer и цифровая подпись скриптом не создаются. `build.rs` встраивает UI/изображения, генерирует ICO из `assets/decorations/project-horse.png` и Windows version info.

## Правила работы с проектом

- UI, системные интеграции и хранение пользовательских настроек держите в `apps/desktop`; сетевую, установочную и игровую логику — в `launcher-core`.
- Сохраняйте разделение команд и состояния. Длительные операции не должны блокировать Slint event loop.
- Публичные команды/состояния принадлежат `launcher-core`; не переносите их в пустой `launcher-shared` только ради общего названия.
- При изменении загрузок сохраняйте проверку путей/размеров/контрольных сумм, очистку временных файлов и обработку отмены. Не записывайте токены в логи.
- Для тестов и пробных установок используйте временные каталоги или `LAUNCHER_DATA_DIR`, не пользовательскую папку игры.
- Проверяйте изменения подходящими существующими тестами. Для сетевой логики используйте локальные fixtures/HTTP-сервер; для UI дополнительно проверяйте отображение и привязку состояний.
- Не редактируйте сгенерированные файлы `target/` и `dist/` как исходники. При изменении дистрибутива синхронизируйте `packaging/README.txt` и release-скрипт.
- Сначала сверяйтесь с кодом: список ограничений выше описывает текущую реализацию, а не план будущих функций.

### Поиск по репозиторию

Предпочитайте `tgrep` для многократного поиска по содержимому. Перед первым общим поиском вызовите `tgrep status .`; используйте существующий сервер. При необходимости запускайте `tgrep serve .` один раз фоновым скрытым процессом (`Start-Process -WindowStyle Hidden` на Windows) и дождитесь успешного статуса.

Форма вызова: `tgrep <flags> -- <pattern> <path>`. Для литералов используйте `-F`, ограничивайте область путём/типом, вывод — `-l`, `-m` и небольшим `-C`. Для машинной обработки — `--json`; exit code 1 означает отсутствие совпадений, 2 — ошибку; предупреждения stderr не скрывайте.

Для только что изменённых файлов используйте `tgrep --no-index` или `rg`. Если tgrep отсутствует, индекс неполон либо поиск одноразовый по конкретному файлу, допустим `rg`. После массовых изменений обновляйте индекс. Не добавляйте `.tgrep/` в отслеживаемый `.gitignore` только для локальной работы агента.
