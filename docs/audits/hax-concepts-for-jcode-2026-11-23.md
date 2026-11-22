# Четыре идеи из Hax для Rust Jcode

**Рекомендация:** сначала сделать повторяемый терминальный acceptance, затем короткий путь для llama.cpp. Инспектор запроса полезен при разборе compaction и cache misses, но только с безопасным представлением данных. Нативный scrollback стоит проверить отдельным экспериментом, а не менять основной интерфейс.

**Срез:** [Jcode `5722088c0fad470072ddfec3026c349bab969e41`][j], [Hax upstream `master`/HEAD `84b5f8d0e47031edceca37c4bccc4c924de7ed9b`][h]; опубликованный 12 ноября 2026 года [v0.7.0][release] указывает на [`1ddc04100a5fd4c75a4eb60992c126814961e391`][tag]. HEAD и цель тега сверены с Git remote, релиз с GitHub API. Ниже выводы по HEAD, не обещания README. Сверены [аудит инструментов][tool-audit] и [Codex CLI parity][codex-audit].

## 1. Терминальный acceptance на одной сценарной оснастке

**Отличие.** Hax [подаёт скрипт через настоящий provider seam][h-mock]: частичные UTF-8 дельты, паузы, фазы tool call и отмена идут через обычный агент и рендер. Его [общий harness][h-tests] открывает отдельный tmux на уникальном сокете, ждёт отрисованных строк и подтверждает resize через PTY. Одна группа [включена в Meson][h-meson] и покрывает REPL, resize, отмену, compaction и CLI. В Jcode уже есть [MockProvider][j-mock], [реальный PTY smoke][j-pty] и [Rust e2e в CI][j-ci]. Пробел не в наличии mock или PTY, а в общем обязательном пути **поток модели → daemon → настоящий TUI → видимый экран**. Это не проверка stdout/schema, уже предложенная в Codex-аудите.

**Встраивание и scope (S).** Собрать общий runner из существующих fixtures в `tests/e2e`: локальный HTTP/SSE, который отдаёт сценарий по этапам, рабочие `serve` и TUI из текущего build, изолированные `JCODE_HOME`/HOME и сокеты, `JCODE_TEST_SESSION=1`. tmux или эквивалент с настоящей эмуляцией: команды отправлять по условию готовности, а не по `sleep`; сохранять только синтетические viewport/PTY/daemon логи при сбое. Один CI job на Unix. Не поставлять второй mock provider в production и не портировать DSL Hax.

**Приёмка.** Два сценария: стрим Markdown и tool status → resize 80→40 колонок → завершение без дублей; Ctrl+C во время следующего незавершённого SSE → нет поздней дельты, prompt снова принимает ввод, replay даёт один новый ответ. Сверять видимые строки и факт вызова fixture. Не менее 20/20 прогонов каждого сценария, целевой бюджет ≤10 с на запуск; намеренно испорченный порядок событий или отрисовка дубля должны уронить проверку.

## 2. llama.cpp: не ещё один транспорт, а понятный локальный маршрут

**Отличие.** Hax [`llamacpp`][h-llama] знает loopback по умолчанию (порт `9931` в [конфиге этого SHA][h-config]), берёт первый ID у single-server или единственную активную модель router, нормализует alias, а при неоднозначном router оставляет выбор пользователю. Список знает `loaded`/`loading`/`sleeping` и failed; выбранный runtime probe [`/props?model=…`][h-props] уточняет `default_generation_settings.n_ctx` и `modalities.vision` по фактически загруженному проектору. Прогрев здесь побочный эффект `/props` на router, не отдельный POST `/models/load`; [смена модели обновляет probe][h-pick]. Jcode уже [подключает OpenAI-compatible endpoint без обязательного ключа на localhost][j-profile], [получает `/models`][j-fetch], разбирает [вложенное `meta.n_ctx` и каталожные модальности][j-meta]. Добавка Hax: штатный вход без ручного профиля, состояние router, выбранная модель и её runtime capabilities, а не базовая совместимость.

**Встраивание и scope (S–M).** Явная встроенная строка `llamacpp` в существующих compatible profiles, переиспользовать `jcode-provider-openrouter-runtime`, `ModelRoute` и [set/refresh сервера][j-switch]. Только для явно выбранного профиля: пассивный каталог со статусом, осторожный auto-select и фоновый `/props` после выбора маршрута, если сервер это поддерживает. Не пробовать `/props` при наведении/обходе списка: это может загрузить модель. `unknown` не равно `false`; разделить живые данные и оценку из каталога. Сохранять last-good отдельно для endpoint; force refresh проходит существующим путём и обновляет открытый picker. Не сканировать порты/сеть, не переключаться молча на облако или другую модель при явном выборе.

**Приёмка.** Loopback fixture с неизвестным Jcode ID: single-model идёт до запроса без `--model`; две idle router-модели не вызывают ни одного `/props` при открытии/закрытии picker; после подтверждения B ровно один запрос про B, ноль про A. Каталог говорит 2k, `/props` для B говорит 4k и `vision=false`: UI и route используют 4k и подтверждённое `false`. Подменить свежий каталог успешным refresh с новым model ID, затем ответом 500/битым JSON: новый ID остаётся доступен в подключённой сессии, состояние помечается устаревшим.

## 3. «Что реально ушло модели»: инспектор выбранного запроса

**Отличие.** Hax [Ctrl+T][h-view] показывает model-facing окно: system, tools и живой суффикс после compaction, а не весь журнал. Это представление контекста приложения, **не** сериализованный provider payload. Дополнительный [HTTP trace][h-wire] фиксирует параметры запроса, статусы и разобранные SSE с временем. Jcode [`/transcript` открывает файл сессии][j-transcript], [`/context` раскладывает состав и размер][j-context], а [canonical capture и wire fingerprints][j-hash] хранят хеши, не читаемое содержимое запроса. Польза Hax: на одном конкретном запросе увидеть system, tool schemas, порядок блоков и фактическую границу compaction, не угадывая по истории или хешам.

**Встраивание и scope (M).** Сначала только Chat Completions в [цикле transport retries][j-retry] существующего OpenAI-compatible runtime: opt-in кольцо ограниченного размера в памяти с session/turn/attempt ID, захват [финального body перед отправкой][j-send] на каждой попытке и метаданные принятых SSE (тип, время, размер, категория ошибки). Показать read-only вид из `/context`; не передавать в модель, телеметрию или обычные логи. Заголовки только по allowlist, фильтр JSON и явных credential values, URLs без query/credentials, изображения и непрозрачные поля только как метаданные. Тело SSE/WS не сохранять на первом этапе. Возможный экспорт позднее только по явному действию, mode `0600`; помечать обрезку и маскировку. Для Responses WebSocket позднее явно разделять wire delta и эффективный вход с подтверждённым префиксом: не реконструировать «точный» запрос из сессионного JSON.

**Приёмка.** Loopback сохраняет принятое body. Инспектор даёт то же дерево JSON с тем же порядком массивов после применения оговорённых redaction/limits; проверить запрос после compaction и повторную попытку с отдельным ID. Синтетический ключ в custom header, URL, JSON-строке и повторённом ответе встречается в данных инспектора, его диагностических логах и будущем экспорте **0 раз**; соседняя сессия не видит запись, при выключенной записи нет вовсе. [Trace Hax][h-trace] взять как идею, не как реализацию безопасности: в этом SHA status/error body и request body фильтруются по зарегистрированным значениям, но SSE data идёт напрямую в formatter, а URL печатается целиком.

## 4. Опциональный primary-screen режим для линейного чата

**Отличие.** Hax оставляет законченные строки терминалу; [текущий хвост Markdown][h-wrap] и [живые области ввода/footer][h-region] обновляет адресно и учитывает переносы по ширине. Scrollback принадлежит терминалу, а не приложению. Jcode [входит в alternate screen][j-terminal] и уже умеет [частичный redraw idle/spinner][j-draw]. Поэтому идея не «добавить diff» и не «C быстрее Rust»; добавка для SSH/tmux: стандартный поиск и копирование истории, которую можно видеть и после выхода Jcode.

**Встраивание и scope (M, эксперимент).** Явный режим только для линейного Unix-чата: terminal lifecycle в `jcode-tui-style`, вывод в существующем `jcode-tui` из принятых событий и текущего composer, без второго agent loop. Committed text прокручивается один раз, хвост, прогресс и composer живут в ограниченной области. Не вмешиваться в основной full-screen режим и не переносить панели, picker, Mermaid или swarm layout в MVP. При resume печатать ограниченный хвост с явной меткой, не выгружать всю историю в terminal history автоматически; предупредить, что видимый scrollback переживает выход.

**Приёмка.** В своём tmux 80×24 прогнать 300 строк mixed-width/Markdown и resize до 40×16: старый и финальный маркеры встречаются по одному в `capture-pane -S -`, shell prompt после выхода виден, alternate screen выключен; Enter/Ctrl+C восстанавливают ожидаемое состояние терминала. На 100 дельтах вводить текст одновременно: ноль глобальных clear-screen/clear-scrollback и повторных записей завершённых строк. Зафиксировать bytes→PTY и p95 key-to-paint на одинаковом fixture с обычным TUI, не заявлять ускорение без парных данных.

## Что проверено запуском

- macOS arm64, Hax из закреплённого исходника, Meson build и Jansson v2.14.1 в scratch. Без установки Hax в PATH, с чистой средой, отдельными HOME/workspace и tmux-сокетами: `meson test --no-rebuild 'e2e/*' providers/llamacpp providers/wire trace transcript terminal/live_region` дал **14/14** групп. Есть [one-shot тесты обеих форм вывода и resume][h-oneshot-test].
- Отдельный fixture `127.0.0.1` на случайном порту: Hax без model ID вызвал `/v1/models`, `/props?model=…` и `/v1/chat/completions` с найденным ID; `-p` вернул только ответ в stdout. Псевдоключ убран из заголовка и request body, но обнаружен в synthetic SSE trace. Jcode с `--version` на `5722088c0`, отдельными `JCODE_HOME`, тестовым флагом и сокетом: `run --json` через compatible endpoint с явным model ID дал один JSON-отчёт с ожидаемым `text`; `/props` не вызывался. Это не проверка всех способов выбора Jcode-модели.
- В отдельном tmux 80×12 при 28 строках Hax первый маркер пропал из viewport, сохранился ровно один раз в native history, `alternate_on=0`. Это проверка поведения, не замер redraw, памяти или скорости. Реальные LLM, платные API, загрузка GGUF, Linux/Windows и предлагаемые изменения Jcode не запускались.

## Что не брать

- Не считать отдельной находкой stdin, чистый stdout, конечный артефакт, JSONL или приглашение запускать внешние программы. [Hax one-shot][h-oneshot] интересен тем, что даёт записи сессии и итоговый `result` в одном потоке; у Jcode уже есть [`--json`/`--ndjson`][j-ndjson] и штатные инструменты, а stdout/stderr/exit status уже в [плане Codex-аудита][codex-audit]. Заимствовать как acceptance для этого плана, не менять формат NDJSON и не делать дисковый формат сессии публичным только ради Hax. Идеи из аудита инструментов про `read`, параллельные вызовы и `edit` anchors здесь не повторяются.
- Не брать C-порт, замену agent loop, daemon/SDK или tool registry, отказ от MCP/swarm, sandbox/approvals. Не копировать process-wide plaintext trace, не отключать fullscreen по умолчанию и не превращать native history в источник истины сессии.

[h]: https://github.com/OleksandrChekhovskyi/hax/commit/84b5f8d0e47031edceca37c4bccc4c924de7ed9b
[tag]: https://github.com/OleksandrChekhovskyi/hax/commit/1ddc04100a5fd4c75a4eb60992c126814961e391
[release]: https://github.com/OleksandrChekhovskyi/hax/releases/tag/v0.7.0
[j]: https://github.com/xenking/jcode/commit/5722088c0fad470072ddfec3026c349bab969e41
[tool-audit]: https://github.com/xenking/jcode/blob/5722088c0fad470072ddfec3026c349bab969e41/docs/audits/tool-runtime-comparison-2026-11-22.md#L23-L47
[codex-audit]: https://github.com/xenking/jcode/blob/5722088c0fad470072ddfec3026c349bab969e41/docs/audits/codex-cli-parity-2026-11-23.md#L85-L92
[h-mock]: https://github.com/OleksandrChekhovskyi/hax/blob/84b5f8d0e47031edceca37c4bccc4c924de7ed9b/src/providers/mock.c#L44-L141
[h-tests]: https://github.com/OleksandrChekhovskyi/hax/blob/84b5f8d0e47031edceca37c4bccc4c924de7ed9b/tests/e2e/harness.py#L327-L445
[h-meson]: https://github.com/OleksandrChekhovskyi/hax/blob/84b5f8d0e47031edceca37c4bccc4c924de7ed9b/tests/meson.build#L180-L205
[h-llama]: https://github.com/OleksandrChekhovskyi/hax/blob/84b5f8d0e47031edceca37c4bccc4c924de7ed9b/src/providers/llamacpp.c#L101-L224
[h-config]: https://github.com/OleksandrChekhovskyi/hax/blob/84b5f8d0e47031edceca37c4bccc4c924de7ed9b/src/config.c#L239-L244
[h-props]: https://github.com/OleksandrChekhovskyi/hax/blob/84b5f8d0e47031edceca37c4bccc4c924de7ed9b/src/providers/llamacpp.c#L233-L326
[h-pick]: https://github.com/OleksandrChekhovskyi/hax/blob/84b5f8d0e47031edceca37c4bccc4c924de7ed9b/src/select.c#L669-L739
[h-view]: https://github.com/OleksandrChekhovskyi/hax/blob/84b5f8d0e47031edceca37c4bccc4c924de7ed9b/src/agent.c#L307-L320
[h-wire]: https://github.com/OleksandrChekhovskyi/hax/blob/84b5f8d0e47031edceca37c4bccc4c924de7ed9b/src/transport/http.c#L240-L299
[h-trace]: https://github.com/OleksandrChekhovskyi/hax/blob/84b5f8d0e47031edceca37c4bccc4c924de7ed9b/src/trace.c#L214-L315
[h-wrap]: https://github.com/OleksandrChekhovskyi/hax/blob/84b5f8d0e47031edceca37c4bccc4c924de7ed9b/src/render/markdown_wrap.c#L185-L255
[h-region]: https://github.com/OleksandrChekhovskyi/hax/blob/84b5f8d0e47031edceca37c4bccc4c924de7ed9b/src/terminal/live_region.c#L63-L129
[h-oneshot-test]: https://github.com/OleksandrChekhovskyi/hax/blob/84b5f8d0e47031edceca37c4bccc4c924de7ed9b/tests/e2e/test_oneshot.py#L15-L236
[h-oneshot]: https://github.com/OleksandrChekhovskyi/hax/blob/84b5f8d0e47031edceca37c4bccc4c924de7ed9b/src/oneshot.c#L93-L231
[j-mock]: https://github.com/xenking/jcode/blob/5722088c0fad470072ddfec3026c349bab969e41/tests/e2e/mock_provider.rs#L12-L100
[j-pty]: https://github.com/xenking/jcode/blob/5722088c0fad470072ddfec3026c349bab969e41/tests/e2e/expand_badge_headless.py#L70-L151
[j-ci]: https://github.com/xenking/jcode/blob/5722088c0fad470072ddfec3026c349bab969e41/.github/workflows/ci.yml#L437-L445
[j-profile]: https://github.com/xenking/jcode/blob/5722088c0fad470072ddfec3026c349bab969e41/crates/jcode-base/src/provider_catalog.rs#L68-L145
[j-fetch]: https://github.com/xenking/jcode/blob/5722088c0fad470072ddfec3026c349bab969e41/crates/jcode-provider-openrouter-runtime/src/catalog_fetch.rs#L41-L111
[j-meta]: https://github.com/xenking/jcode/blob/5722088c0fad470072ddfec3026c349bab969e41/crates/jcode-provider-openrouter-runtime/src/models_catalog_parse.rs#L162-L205
[j-switch]: https://github.com/xenking/jcode/blob/5722088c0fad470072ddfec3026c349bab969e41/crates/jcode-app-core/src/server/provider_control.rs#L412-L490
[j-transcript]: https://github.com/xenking/jcode/blob/5722088c0fad470072ddfec3026c349bab969e41/crates/jcode-tui/src/tui/app/commands.rs#L1476-L1520
[j-context]: https://github.com/xenking/jcode/blob/5722088c0fad470072ddfec3026c349bab969e41/crates/jcode-app-core/src/agent/context_breakdown.rs#L41-L98
[j-hash]: https://github.com/xenking/jcode/blob/5722088c0fad470072ddfec3026c349bab969e41/crates/jcode-provider-core/src/fingerprint.rs#L365-L440
[j-retry]: https://github.com/xenking/jcode/blob/5722088c0fad470072ddfec3026c349bab969e41/crates/jcode-provider-openrouter-runtime/src/openrouter_sse_stream.rs#L91-L163
[j-send]: https://github.com/xenking/jcode/blob/5722088c0fad470072ddfec3026c349bab969e41/crates/jcode-provider-openrouter-runtime/src/openrouter_sse_stream.rs#L248-L304
[j-terminal]: https://github.com/xenking/jcode/blob/5722088c0fad470072ddfec3026c349bab969e41/crates/jcode-tui-style/src/lib.rs#L82-L97
[j-draw]: https://github.com/xenking/jcode/blob/5722088c0fad470072ddfec3026c349bab969e41/crates/jcode-tui/src/tui/app/run_shell.rs#L506-L575
[j-ndjson]: https://github.com/xenking/jcode/blob/5722088c0fad470072ddfec3026c349bab969e41/src/cli/commands.rs#L2645-L2676
