# Аудит системных промптов: Codex CLI, Claude Code, Cursor и др. против jcode, и промпты по ролям моделей

Дата: 24 ноября 2026. jcode `7c45460b4`. Сырые выводы и скрипты лежат в
[`prompt-audit-2026-11-24/`](prompt-audit-2026-11-24/).

## Вопрос

Нужна ли jcode возможность задавать свой промпт, набор инструментов и политику
контекста для класса модели или роли, и как это сделать, опираясь на то, как
устроены другие harness'ы и что показывают наши собственные сессии. Роли:

- **orchestrator**: управляет воркерами и почти не трогает файлы сам (аналог `/vibe` director в omp).
- **advisor/researcher**: дорогая модель, которой нельзя засорять контекст выводом инструментов.
- **fast**: Haiku, Luna, DeepSeek Flash. Механическая работа, большой объём.
- **general**: Terra, Sonnet, DeepSeek Pro. Обычная работа по умолчанию.

## Источники и метод

| Источник | Срез | Что взято |
| --- | --- | --- |
| `Piebald-AI/claude-code-system-prompts` | `659d9f4`, Claude Code v2.1.330 | 1293 фрагмента с их собственными подсчётами токенов |
| `openai/codex` | `96681d8` | `codex-rs/models-manager/models.json` (каталог с промптами по моделям), `core/*_prompt.md`, шаблоны multi-agent |
| `x1xhlol/system-prompts-and-models-of-ai-tools` | `162db92` | Cursor (утёкшие версии 2025 года), VS Code Copilot, Amp, Augment, Windsurf, opencode и др. |
| `sst/opencode` | `30ef864` | `packages/core/src/plugin/optimize.ts`, промпты по семействам моделей |
| `can1357/oh-my-pi` | `4df6534` | `modelRoles`, `/vibe`, advisor, bundled agents `task`/`sonic` |
| jcode, живой daemon | `agent:context:prepare` для двух headless-сессий | Собранный промпт и реально отправляемые схемы инструментов |
| jcode, `~/.jcode/sessions` | 709 сессий с 1 по 24 ноября | `token_usage` каждого запроса, вызовы инструментов, размеры их результатов |

Токены считаются `tiktoken o200k_base`, если не сказано иное. Для одних и тех
же 1293 файлов Piebald насчитал 657 271 токен, а o200k 422 877, то есть в 1,55
раза больше. Это совпадает с нашими сессиями: префикс jcode в o200k около
12,5–16,2 тыс., а Anthropic в первом запросе Opus сообщает медиану 24,9 тыс.
Поэтому числа Piebald я привожу как есть и помечаю `tks`, а сравнение между
harness'ами веду в o200k.

Ограничения. Промпты Cursor в x1 датированы 2025 годом и могут не отражать
текущий продукт. Телеметрия только моя, за три недели, и сильно смещена в
сторону Opus как root и GPT-6.x Sol и Terra как воркеров. Классы моделей
определены регуляркой по имени (`measure_sessions.py: klass`).

## Как jcode собирает промпт сейчас

`crates/jcode-base/src/prompt.rs`, `build_system_prompt_split_with_sources`:

1. База: `system_prompt.md` (420 ток.) или полная замена из `./.jcode/system-prompt.md`, затем `~/.jcode/system-prompt.md`.
2. Модули возможностей: Mermaid (12), Codemode (204).
3. Self-dev или Desktop self-dev (804).
4. `AGENTS.md` проекта и глобальный.
5. `prompt-overlay.md` и `preferred-tools.md` из `~/.jcode` и `./.jcode` (добавляются, не заменяют).
6. Список скиллов (2930 токенов на этой машине).
7. Динамическая часть: память, активный скилл, screen context.
8. SDK `system_prompt_append` на сессию (так сделан Jarvis).

Промпт замораживается при первом запросе, изменения файлов приходят
`<system-reminder>`. Swarm-инструкции (`swarm_prompt.md`) встроены в описание
инструмента `swarm`, а не в системный промпт.

Чего нет:

- Ничего не зависит от модели. Провайдеры OpenAI и Anthropic получают один и тот же текст, проверено поиском по `jcode-provider-*`.
- Ничего не зависит от роли. Воркер swarm получает тот же системный промпт и тот же набор инструментов, что и root. `agents.*` управляет только выбором модели, effort, скорости и пулом (`swarm_model`, `swarm_pool`, `swarm_effort_caps`, `title_model`, `memory_model`).
- Нет политики контекста по роли: нельзя запретить root'у тяжёлые инструменты или вычищать устаревшие результаты для advisor'а.

### Замер собранного префикса

| Срез | Системный промпт | Схемы инструментов | Итого |
| --- | --- | --- | --- |
| Обычная сессия, пустой проект | 4008 | 21 инструмент, 8482 | **12 490** |
| Self-dev в этом репозитории | 6717 | 23 инструмента, 9489 | **16 206** |

Крупнейшие куски: список скиллов 2930 (23% префикса обычной сессии), схема
`todo` 1819, `swarm` 1080, `macos_computer_use` 884, `desktop` 687. Ещё 53
инструмента (в основном MCP) отложены через `load_tools` и в префикс не
попадают, без этого было бы 28 439.

## Как это устроено у других

| Harness | Промпт по модели | Промпт по роли | Сужение инструментов по роли | Контекст дорогой модели |
| --- | --- | --- | --- | --- |
| **Codex CLI** | Да, `instructions_template` в каталоге моделей, приходит с сервера | `multi_agent.role.root` и `.subagent`, review-агент с отдельным `base_instructions` | Не видно в каталоге | Нет отдельной политики |
| **Claude Code** | Только модуль идентичности модели (Fable 5, 5.1, 5.2) | Explore, Plan, general-purpose, coordinator, worker, advisor | Explore и Plan только чтение, coordinator работает через Agent, SendMessage, TaskStop | Advisor получает всю историю целиком |
| **Cursor** (2025) | Нет данных | Agent и Chat | Нет данных | Нет данных |
| **VS Code Copilot** | Да, отдельный файл на модель | Нет | Нет | Нет |
| **Amp** | Да, Claude и GPT-5 отдельно | Нет данных | Нет данных | Нет данных |
| **opencode** | Да, плагины по семейству: GPT, Astra, Kimi, Trinity, Meta заменяют, Anthropic дописывает | `system` агента важнее промпта модели | Через permission агента | Нет |
| **omp** | Нет, роль выбирает модель | `modelRoles` (`default`, `smol`, `slow`, `plan`, `task`, `advisor`, ...), bundled agents | Director в `/vibe` видит только `read`, `todo` и пять `vibe_*` | Advisor видит дельты, старые `read`/`grep`/`glob` выселяются |
| **jcode** | Нет | Нет | Нет | Нет |

### Codex: промпт приходит вместе с моделью

`models.json` хранит для каждой модели `instructions_template` и модули
`persistent_instructions`, `multi_agent`, `token_budget`,
`collaboration_modes`, `confirmation_policies`, `auto_review`. Размеры
шаблона (o200k):

| Модель | Шаблон | multi_agent | effort по умолчанию |
| --- | --- | --- | --- |
| gpt-6-luna | 3452 | 572 | medium |
| gpt-6-sol | 3631 | 572 | medium |
| gpt-6.1-terra | 4079 | 572 | low |
| gpt-6.1-sol | 4143 | 572 | low |
| gpt-6.2-sol | 5059 | 572 | low |

Наблюдения:

- Быстрой модели Codex даёт **самый короткий** промпт и **более высокий** effort по умолчанию, сильной наоборот. Для Luna нет разделов про стиль технических текстов и PR, у Sol 6.2 добавлены Frontend Design Guidance и Coding Style.
- Между Luna и Terra изменилось 77 строк, между Terra и Sol 6.2 145. Это переписанные абзацы о стиле письма и автономии, а не просто добавленные разделы.
- Роль задаётся отдельно от модели: `multi_agent.role.root` начинается с «You are `/root`, the primary agent in a team», а `subagent` объясняет, что финальный ответ уходит родителю.
- Пользовательская замена базы делается через `model_instructions_file`, как наш `system-prompt.md`.

Вывод для jcode: промпт как свойство записи каталога сочетается с нашим
правилом «каталог моделей не равен allowlist». Профиль можно сопоставлять по
шаблону ID и не перечислять модели в коде.

### Claude Code: модули и отдельные промпты агентов

Claude Code собирает промпт из сотен условных фрагментов. Для ролей важны
(подсчёты Piebald):

| Промпт | tks | Суть |
| --- | --- | --- |
| Coordinator mode orchestration | 5921 | Роль coordinator, фазы Research, Synthesis, Implementation, Verification, правило «Always synthesize», continue или spawn |
| Coordinator worker instructions | 830 | Делай только порученное, коммить только свои файлы, отчёт для coordinator'а |
| Explore | 862 | Только чтение, «fast agent», параллельные вызовы grep/read |
| Plan (enhanced) | 1066 | Только чтение, план |
| general-purpose | 446 | Полный набор инструментов |
| Advisor tool instructions | 443 | Звать advisor до существенной работы и перед «готово» |

Explore раньше по умолчанию работал на Haiku, но с v2.1.198 наследует модель
основной сессии, на Claude API не выше Opus. Модель агента берётся из
параметра вызова, затем из frontmatter, затем из `CLAUDE_CODE_SUBAGENT_MODEL`,
затем из модели основной сессии
([документация](https://code.claude.com/docs/en/sub-agents)).

Advisor в Claude Code реализован противоположно задаче «не засорять дорогую
модель»: это инструмент без параметров, и стоит его вызвать, как более сильной
модели пересылается вся история со всеми вызовами инструментов.

### opencode: замена по семейству, агент важнее

`optimize.ts` подбирает текст по подстроке ID модели: `gpt` получает
`gpt.txt` (563 ток.), `astra` получает `gpt-astra.txt` (794), `kimi` 1804,
`trinity` 1660, `muse` 1613. Это **замена** базового промпта. Для `claude`
**дописывается** 59 токенов. Если у агента задан свой `system`, промпт модели
не применяется. Порядок получается такой: агент, затем семейство модели,
затем умолчание.

### VS Code Copilot и Amp: отдельный файл на модель

У VS Code сходство строк с промптом `claude-sonnet-4` равно 0,77 для Gemini
2.5 Pro, 0,70 для GPT-4.1, 0,69 для GPT-5, 0,65 для GPT-5 mini и 0,59 для
GPT-4o. GPT-5 и GPT-5 mini получают промпты в 2,5 раза длиннее (5262 и 5181
против 2027). У Amp промпты Claude (13 101) и GPT-5 (11 871) написаны
независимо и построчно не совпадают.

### omp: роль как ось конфигурации

- `modelRoles` связывает роль с моделью и effort (`openai-codex/gpt-5.5:xhigh`), у каждой роли своя цепочка fallback.
- Bundled agents: у `task` модель `@task`, effort `auto`, у `sonic` **тот же промпт** `task.md` (172 ток.), но модель `@smol` и effort `medium`. Роль меняет модель и бюджет, текст общий.
- `/vibe` director (444 ток.): «NEVER edit, run, grep, or build yourself. Verify work by reading files». Инструменты урезаны до `read`, `todo` и пяти `vibe_*`. Воркеры двух уровней: `fast` (`sonic`, `@smol`) и `good` (`task`).
- Advisor (1177 ток.) по умолчанию только читает (`read`, `grep`, `glob`). Ему приходят инкрементальные дельты транскрипта, по 2–3 вызова на совет. `tool-result-eviction.ts` выселяет из его контекста устаревшие результаты `read`/`grep`/`glob`, выбирая место среза так, чтобы сэкономленное перевесило перезапись кэша. `delta-split.ts` дробит дельты на отдельные сообщения, чтобы рос `cache_read` (в комментарии: 11066, затем 11091, затем 11112).

## Что показывают наши сессии

709 сессий с 1 по 24 ноября, без headless debug-сессий. Сырьё:
[`sessions_report.txt`](prompt-audit-2026-11-24/sessions_report.txt),
[`carried_report.txt`](prompt-audit-2026-11-24/carried_report.txt).

| Класс и роль | Сессий | Запросов | Первый запрос, p50 | Промпт на запрос, p50 / p90 | Доля всех prompt-токенов | Вызовов на ход пользователя, p50 |
| --- | --- | --- | --- | --- | --- | --- |
| frontier, root (Opus 5.5 и др.) | 167 | 27 984 | 24,9K | 239K / 412K | **47%** | 1,0 |
| frontier, worker (Sol 6.x и др.) | 329 | 38 147 | 27,4K | 133K / 226K | 36% | n/a |
| general, worker (Terra 6.1) | 164 | 18 915 | 18,7K | 108K / 185K | 14% | n/a |
| fast, worker (DeepSeek Flash) | 28 | 3102 | 27,9K | 160K / 289K | 3% | n/a |

Всего около 14,9 млрд prompt-токенов, медиана доли кэша 0,98–0,99.

Как распределяются вызовы инструментов:

- **frontier root**: `bash` 54%, `bg` 14%, `swarm` 10%, `todo` 5%, `codemode` 4%, `edit` 4%. Дорогой root в основном сам ходит в shell.
- frontier worker: `bash` 35%, `codemode` 13%, `bg` 10%, `read` 10%.
- fast worker: `bash` 67%, `apply_patch` 8%, `read` 8%.

Сколько промпта занимают уже полученные результаты инструментов. При резком
падении размера промпта счётчик сбрасывается, это считается компакцией.
Результат ограничен сверху фактическим `prompt_tokens`:

| Класс и роль | Доля промпта под старыми результатами | Основные источники |
| --- | --- | --- |
| frontier root | **26%**, около 1,8 млрд токенов | `bash` 35%, **`todo` 32%**, `bg` 7%, `read` 6% |
| frontier worker | 55% | `codemode` 41%, `bash` 18%, `batch` 12% |
| general worker | 55% | `bash` 40%, `batch` 17%, `read` 12% |
| fast worker | 50% | `bash` 60%, `read` 22% |

Размер одного результата (символы):

| Инструмент | Вызовов | p50 | p90 |
| --- | --- | --- | --- |
| bash | 50 478 | 688 | 5635 |
| codemode | 6430 | 6205 | 28 660 |
| **todo** | 3530 | 1986 | **39 249** |
| batch | 3688 | 6667 | 22 474 |
| read | 6929 | 3466 | 8929 |
| apply_patch | 4807 | 3221 | 8433 |

### Выводы из данных

1. **Дорогая модель как root тратит почти половину бюджета и сама работает руками.** 47% всех prompt-токенов уходят на frontier root, медиана 239K на запрос, больше половины его вызовов приходится на `bash`. Роль orchestrator с урезанными инструментами, как director в omp, бьёт в самый большой кусок расходов.
2. **`todo` вторым по величине засоряет контекст root'а.** На p90 один результат весит 39K символов, а у frontier root на `todo` приходится 32% накопленного вывода инструментов. Вероятно, инструмент возвращает весь план с историей уверенности после каждого изменения (так выглядят и ответы в этой сессии). Это исправляется без всяких ролей: возвращать короткую сводку и diff.
3. **Префикс не главная проблема.** 12–16 тыс. токенов o200k при медиане 108–239K на запрос и доле кэша 0,98. Экономия на тексте промпта для fast-моделей даст проценты. Хвост из старых результатов даёт 26–55%.
4. **Fast-воркеры не дешевле в токенах на запрос.** У DeepSeek Flash медиана 160K и 59 запросов на одно сообщение координатора. Им нужен короткий жёсткий бриф и ранняя отдача результата, а не просто дешёвая модель.
5. **Effort воркеров почти всегда `high`**: 164 из 164 у general, 28 из 28 у fast, 224 из 329 у frontier. Codex, напротив, даёт сильным моделям `low`, а быстрым `medium`. Это стоит проверить A/B-тестом. Готовых данных у нас нет.

## Предлагаемый дизайн

### Ось 1: профиль роли

Роль отвечает на вопрос «кем модель работает в этой сессии». Это главный
рычаг: именно роль меняет инструменты и политику контекста.

```toml
[prompt.roles.orchestrator]
# replace | append. append дописывает к системному промпту по умолчанию
mode = "append"
file = "~/.jcode/prompts/orchestrator.md"
# Жёсткий allowlist, отсечение в tool registry, а не просьба в тексте
tools = ["read", "agentgrep", "todo", "swarm", "bg", "ask", "user_response"]
context = { evict_tool_results = ["read", "agentgrep", "bash"], keep_last_reviews = 1 }
effort = "low"

[prompt.roles.advisor]
mode = "replace"
file = "~/.jcode/prompts/advisor.md"
tools = ["read", "agentgrep"]
# получает дельты транскрипта, а не всю историю
context = { input = "transcript_deltas", evict_tool_results = ["read", "agentgrep"] }

[prompt.roles.fast]
mode = "append"
file = "~/.jcode/prompts/fast.md"   # коротко: делай бриф, без исследования, ранний отчёт
skills = false                       # минус 2930 токенов списка скиллов
tools_exclude = ["swarm", "macos_computer_use", "desktop", "applet"]

[prompt.roles.general]
# умолчание, текущий промпт
```

Привязка ролей:

- root: `orchestrator`, если включён режим (новый `/orchestrate` или `reasoning_effort = "swarm"`), иначе `general`.
- swarm worker: роль задаёт todo lane или `swarm spawn` (`role = "fast"`). По умолчанию роль берётся из класса модели в `swarm_pool`.
- advisor: отдельная сессия-наблюдатель (это новая функциональность, отдельная задача).

### Ось 2: профиль семейства модели

Отвечает на вопрос «как с этой моделью разговаривать». Только текст, без
инструментов. Сопоставление по glob ID, чтобы новые модели из каталога
подхватывались без релиза:

```toml
[[prompt.models]]
match = ["claude-haiku-*", "*-luna", "deepseek-*-flash"]
append = "~/.jcode/prompts/models/fast-family.md"

[[prompt.models]]
match = ["gpt-6*"]
append = "~/.jcode/prompts/models/gpt.md"
```

### Порядок применения

Повторяем opencode и Claude Code: более конкретное важнее.

1. `session.system_prompt` (SDK, полная замена), затем `system_prompt_append`.
2. Роль в режиме `replace` заменяет базу целиком.
3. База: `system-prompt.md` или встроенная.
4. Профиль семейства модели (append).
5. Роль в режиме `append`.
6. AGENTS.md, overlay, preferred-tools, скиллы (если роль не отключила).

### Ограничения реализации

- **Кэш.** Роль и семейство известны до первого запроса, поэтому их текст попадает в замороженный `static_part`. Смена модели посреди сессии меняет префикс. Значит, профиль семейства применяется при заморозке, а смена модели приходит `<system-reminder>`, как изменения prompt-файлов сейчас (`PromptFiles::change_reminder`).
- **Инструменты.** Allowlist применяется там же, где сейчас `locked_tools`, иначе модель увидит инструмент в схеме и получит отказ при вызове.
- **Отпечатки.** Хэш файлов роли и семейства добавляется в `PromptFileVersions`, тогда горячая перезагрузка работает так же, как для overlay.
- **Учёт.** `ContextInfo` получает поля `role_chars` и `model_profile_chars`, чтобы `/context` показывал их отдельно.
- **Desktop и SDK.** Роль передаётся в `SessionCreate` рядом с `system_prompt_append`.

## Что делать в каком порядке

| # | Шаг | Ожидаемый эффект по данным | Стоимость |
| --- | --- | --- | --- |
| 1 | Сократить результат `todo` до сводки и diff | До 32% накопленного вывода у frontier root, около 8% всех его prompt-токенов | Малая, без новой конфигурации |
| 2 | Роль `orchestrator` с allowlist инструментов для root в swarm-режиме | Основной рычаг для 47% бюджета | Средняя |
| 3 | `[prompt.roles.*]` с `mode`, `file`, `tools`, `skills`, `effort` | Позволяет ставить эксперименты без пересборки | Средняя |
| 4 | `[[prompt.models]]` с glob-сопоставлением | Тонкая настройка стиля, несколько процентов префикса | Малая |
| 5 | Advisor с дельтами и выселением, как в omp | Дорогая модель без шума инструментов | Большая |

## Как проверять

Только статические подсчёты не доказывают качество. Для каждого шага:

1. **Токены и кэш.** Повторить `measure_sessions.py` и `measure_carried.py` на неделе после включения и сравнить медиану prompt-токенов на запрос, долю старых результатов и `cache_read` по классам.
2. **Поведение.** Изолированный `JCODE_HOME` и `JCODE_TEST_SESSION=1` (см. AGENTS.md), набор из 10–20 повторяемых задач из истории self-dev. Для каждой роли: успех по тестам, число запросов, prompt-токены, стоимость. Сравнивать роль с умолчанием на одной и той же модели.
3. **Effort.** Отдельный A/B `low` против `high` для воркеров Terra и Sol на тех же задачах, раз у Codex умолчания противоположны нашим.
4. **Регрессии.** Тесты на порядок применения, на то, что новая модель из каталога попадает в профиль по glob без изменения кода, и на неизменность замороженного префикса при смене роли воркера.

## Воспроизведение

```bash
cd "$JCODE_SCRATCH_DIR/prompts"   # здесь клоны cc/ codex/ x1/ omp/ opencode/
# собрать ctx_plain.json и ctx_dev.json:
#   debug_socket create_session:<path>, затем agent:context:prepare с session_id
uv run --with tiktoken python measure_jcode.py
uv run --with tiktoken python measure_codex.py
uv run --with tiktoken python measure_cc.py
uv run --with tiktoken python measure_x1.py
python3 measure_sessions.py 2026-11-01   # только агрегаты, без текста сессий
python3 measure_carried.py 2026-11-01
```
