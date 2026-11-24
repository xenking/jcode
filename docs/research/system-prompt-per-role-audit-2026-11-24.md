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

Дополнено после разбора ещё трёх harness'ов (см. раздел ниже): `Dicklesworthstone/pi_agent_rust`
`7aae3036`, `deepseek-ai/deepseek-harness` (dsh) `4b399c39`, `Hmbown/DeepSeek-TUI`
`62765f2`.

| Приоритет | Шаг | Основание | У кого взято | Стоимость |
| --- | --- | --- | --- | --- |
| P0 | Результат `todo` сократить до сводки и diff | `tool/todo.rs:1153` при каждом вызове печатает pretty JSON всего списка, плана и целей. Это 32% накопленного вывода у frontier root | Собственные данные | Малая |
| P0 | Руководство по инструментам выводить только для включённых инструментов | `system_prompt.md:27-28` жёстко упоминает `agentgrep`, `codemode`, `batch`. Без этого любой allowlist роли оставит в промпте ссылки на скрытые инструменты | opencode `SessionSystemPrompt.render(tools)`, dsh `PromptSection` на инструмент, pi `default_system_prompt(enabled_tools)` | Малая |
| P1 | Пресеты ролей для swarm-воркеров: `scout`, `task`, `sonic`, `reviewer` | Сейчас воркер получает полный промпт и все 23 инструмента, 55% его промпта занимает старый вывод | pi bundled, DeepSeek-TUI `FleetRolePreset`, dsh agent preset, opencode `explore` | Средняя |
| P1 | Роль `orchestrator` для root с allowlist | 47% бюджета, 54% вызовов в `bash` | omp `/vibe` director, Claude Code coordinator | Средняя |
| P2 | Надстройка по семейству модели по glob ID, не применяется, если у роли свой `replace` | Стиль, не бюджет | opencode `optimize.ts`, Codex `models.json` | Малая |
| P3 | Advisor или consultant по компактному дайджесту хода, только чтение, effort high | Дорогая модель без вывода инструментов | pi `advisor.rs` `TurnDigest`, DeepSeek-TUI `Consultant`, omp дельты и выселение | Большая |

Не берём:

- Полные шаблоны на каждую модель, как в Codex и VS Code. Их авторы сами делают модели. У нас каталог открытый, и поддерживать N копий промпта без eval не окупится.
- Coordinator из Claude Code целиком (5921 tks). Director из omp (444 o200k) даёт тот же контракт за двадцатую часть.
- Advisor из Claude Code, которому пересылается вся история. Это противоположность цели.
- Plugin-архитектуру dsh (Cordis). Из неё берём только модель данных секций.

## Дополнение: opencode, pi_agent_rust, DeepSeek Harness, DeepSeek-TUI

### opencode: агент старше модели

- `Agent.Info` (`packages/schema/src/agent.ts`): `model`, `system` (замена), `instructions` (добавление, форма `{selected, unselected}` сообщает модели, что инструкции перестали действовать), `mode` (`primary`, `subagent`, `all`), `steps`, `permissions`.
- Встроенные агенты (`plugin/agent.ts`): `build` (по умолчанию), `general` (субагент без вопросов и вложенных субагентов), `explore` (запрещено всё, кроме shell, grep, glob, read, webfetch, websearch), а также служебные `compaction`, `title` и `summary`, у которых запрещены все инструменты.
- Промпт по модели (`plugin/optimize.ts`) подбирается по подстроке ID. Для `gpt`, `astra`, `kimi`, `trinity` и `muse` база **заменяется**, для `claude` **дописывается** 59 токенов. Первой строкой стоит `if (agent.system) return`: свой промпт агента отключает промпт модели. Хук срабатывает на `context`, `compaction` и `generate`.
- Руководство по инструментам (`session/system-prompt.ts`) выводится по списку фактически включённых инструментов: строки про shell, write и edit появляются, только если инструмент есть.
- `plugin/verbosity.ts` ставит `textVerbosity = "low"` для gpt-6 и gpt-5.x, если пользователь не задал другое. Это тоже настройка по семейству, только не в тексте.
- Автовычистки старого вывода нет: `compaction.prune` помечен как unsupported. Компакция ступенчатая: сначала обрезаются выводы до 2000 символов, потом удаляются, потом остаётся только текст пользователя.

### pi_agent_rust: роль выбирает модель, профиль выбирает инструменты

- `ModelRole` (`models.rs`): `default`, `smol`, `slow`, `plan`, `commit`, `vision`, `designer`, `task`, `advisor`, `tiny`. Роль задаётся только явно (`{role: "smol"}`) и никогда не выводится из ID модели. Если роль не настроена, запуск падает с ошибкой, молчаливого отката на default нет (`subagents/model_selection.rs`).
- Встроенные профили (`subagents/bundled.rs`) названы как в omp. Это один абзац промпта и список инструментов:
  - `task` и `sonic`: `read,bash,edit,write,grep,find,ls,hashline_edit,jobs`;
  - `scout`, `reviewer` и `security-reviewer`: `read,grep,find,ls`;
  - у всех запрещено делегировать дальше и коммитить без просьбы;
  - финальный ответ должен содержать outcome, изменённые файлы, выполненную проверку и риски.
- Базовый промпт (`app.rs: build_system_prompt`) перечисляет только включённые инструменты. Остальные доступны через диспетчер `xdev`: из 36 инструментов в схеме всегда 14. Это тот же принцип, что наш `load_tools`. Есть `--system-prompt` и `--append-system-prompt`.
- Advisor (`advisor.rs`) работает на своём провайдере и в своём контексте. После каждого хода ему отправляется `TurnDigest`, а не история: до 25 затронутых файлов, до 15 команд, до 10 ошибок инструментов и до 2000 символов финального ответа, с редакцией секретов. Тривиальные ходы пропускаются, после трёх ошибок подряд advisor отключается. Ответ структурирован: `note`, `concern` или `blocker`. Уровень, с которого законченный ход возобновляется, задаёт `ContinueSeverity`. Для сценария «дорогая модель без шума» это самая дешёвая схема из всех изученных.

### DeepSeek Harness (dsh): промпт как набор именованных секций

- `PromptSection {name, order, text, complete, replacement, retirement}` (`docs/subsystems/system-prompt.md`). Из секций с одинаковым именем побеждает ближайшая по scope, так что пресет агента переопределяет только свои секции. Секция с `complete` заменяет весь промпт.
- Персона вынесена в отдельные секции `persona-prefix` и `persona-suffix` с шаблоном `{{model}}`. Пресет субагента меняет одну строку («You are a file reviewer…»), всё остальное переиспользуется.
- У каждого инструмента своя секция с порядком (`TOOL_EDIT` 1300, `TOOL_APPLY_PATCH` 1350, `TOOL_GLOB` 1400), и в промпт она попадает только вместе с инструментом. Замер по snapshot-тестам репозитория (o200k): обычная сессия 737, субагент с фильтром инструментов 460, все инструменты отложены 337, минимальный пресет 8. У jcode только системная часть занимает 4008.
- Изменения промпта посреди сессии идут дельтой: `systemUpdates()` дописывает системное сообщение в историю (`<!-- system/message change N -->` в snapshot'ах team-режима). Полная пересборка бывает только после компакции. У нас так работает `<system-reminder>`, и туда же ложится смена модели или роли.

### DeepSeek-TUI: закрытый набор ролей и открытые пресеты

- `FleetRole` (`agent-contract/src/role.rs`): `worker`, `scout`, `planner`, `reviewer`, `builder`, `verifier`, `consultant`, `custom`. Права заданы кодом (`worker_profile.rs`). Consultant может читать файлы и сеть, shell у него нет, `reasoning_effort = high` по умолчанию, а число шагов ограничено как у read-only ролей.
- Пресеты пользователя (`config/src/fleet.rs: FleetRolePreset`): `tool_profile` (`read-only`, `read-write`, `custom`), `tools`, `capabilities`, `timeout_seconds` и при желании `instructions` как добавка к промпту. Свои пресеты ищутся раньше встроенных.
- Роли workflow (`workflow/src/model_policy.rs`) связаны с моделью через требования к возможностям (`tool_calls`, `json_mode`, `prompt_cache`, `large_context`): модель, не подходящая роли, не назначается.
- Системный промпт в golden-файлах занимает 1251 токен и одинаков в режимах agent и plan. Режим в системный промпт не зашит, так что префикс кэша не ломается.

### Итоговая модель для jcode

Во всех трёх Rust-реализациях и в opencode роль устроена одинаково: **персона плюс список инструментов, права, effort и лимит шагов**. Общий промпт собирается из секций включённых инструментов. Модель влияет на промпт слабее роли и отключается ею. Для jcode это означает:

1. `system_prompt.md` разбить на секции с `name` и `order`, строки про инструменты привязать к самим инструментам (P0).
2. `[agents.roles.<name>]` с полями `persona` или `instructions`, `tools` (или `tool_profile`), `effort`, `max_steps`, `skills`. Встроенные роли: `scout`, `task`, `sonic`, `reviewer`, `orchestrator`, `consultant`. Выбор роли всегда явный, как в pi: через параметр `swarm spawn` или свойство todo lane, без догадок по ID модели (P1).
3. `[[agents.model_prompts]]` с `match` и `append` или `replace`. Не применяется, если у роли своя персона с `replace`, как в opencode (P2).
4. Consultant по `TurnDigest`, как в pi, без shell и с effort high, как в DeepSeek-TUI. Ответ уровня `note`, `concern` или `blocker` приходит в сессию как `<system-reminder>` (P3).

Ранее предложенная секция `[prompt.roles.*]` заменяется на `[agents.roles.*]`, потому что роль управляет не только промптом, а `agents.*` уже содержит выбор модели и effort.

## Реализация P0 и P1

Имя. В jcode слово role уже занято ролью участника swarm (`comm_assign_role`: coordinator, worker), поэтому новая сущность называется **agent profile**: `[agents.profiles.<name>]`, параметр `profile` у `swarm spawn`, поле `lane_profile` у todo lane.

### P0a. Эхо `todo`

Замер по 3328 результатам `todo` с 10 ноября ([`todo_report.txt`](prompt-audit-2026-11-24/todo_report.txt)): поле `description` занимает 80,6% символов вывода, `confidence_history` 5%, секции Plan и Goals вместе 8%. Модель сама написала эти описания и получает их обратно при каждой записи.

- При записи в текст для модели не попадают `description`, `confidence_history` и поля `*_history` в Plan и Goals. Одна задача на строку вместо pretty JSON.
- Чтение (`todo` без изменений) по-прежнему возвращает всё, включая описания.
- Формат остаётся тем, что разбирает TUI-карточка (`parse_todo_tool_output`): JSON-массив, затем метки `Plan:`, `Goals:`, `Plan updates:`, `Goal updates:`. Карточка описания и историю не рисует, проверено поиском по `jcode-tui` и `jcode-desktop`. Полные данные остаются в `metadata` и в хранилище.

### P0b. Строки промпта, зависящие от инструмента

Строка с префиксом `<!-- tools: a, b -->` попадает в промпт, если сессии разрешён хотя бы один из перечисленных инструментов. Сама метка вырезается. Заголовок `##`, под которым не осталось строк, тоже удаляется. Разрешение берётся из той же политики сессии, что ограничивает вызовы (`SESSION_TOOL_POLICIES`: `[tools]`, SDK, профиль), поэтому текст и схемы не расходятся. Модуль Codemode попадает в промпт только тогда, когда разрешён `codemode`. Метки работают и в пользовательском `system-prompt.md`. Без меток текст не меняется.

### P1. Профили агентов

```toml
[agents]
# Профиль для новых root-сессий (не воркеров). По умолчанию не задан.
root_profile = "orchestrator"

[agents.profiles.scout]          # переопределяет встроенный по полям
description = "Read-only exploration"
instructions = "..."            # или instructions_file = "~/.jcode/profiles/scout.md"
tools = ["read", "agentgrep", "bash", "todo", "swarm"]   # allowlist
exclude_tools = []
skills = false                    # без списка скиллов
effort = "medium"                 # если spawn не задал effort
model = "claude-haiku-5-5"        # если spawn не задал model
```

Встроенные профили (тексты в `crates/jcode-base/src/prompt/profiles/*.md`):

| Профиль | Назначение | Инструменты | Скиллы |
| --- | --- | --- | --- |
| `orchestrator` | Root, делегирует и интегрирует, сам не правит и не запускает | read, agentgrep, batch, todo, swarm, ask, user_response, applet, skill_manage, websearch, webfetch, load_tools, selfdev, advisor | да |
| `task` | Реализация по брифу | все, кроме macos_computer_use, desktop, applet, rollout_wait | да |
| `sonic` | Узкие механические правки и сбор данных | read, agentgrep, batch, bash, bg, edit, write, apply_patch, todo, swarm, ask | нет |
| `scout` | Исследование только на чтение, карта с путями и строками | read, agentgrep, batch, bash, bg, todo, swarm, websearch, webfetch, ask | нет |
| `reviewer` | Проверка корректности только на чтение | как у scout | нет |

Инварианты:

1. **Выбор только явный**, как в pi. Источники по приоритету: `swarm spawn profile`, затем `lane_profile` лейна, затем `agents.root_profile` для новой root-сессии. По ID модели профиль никогда не угадывается. Если профиля нет, spawn завершается ошибкой со списком доступных.
2. **Профиль хранится в сессии** (`Session.agent_profile`) и после перезапуска или reload применяется снова из неё, а не из текущего `root_profile`. Для visible-воркеров он записывается при подготовке сессии, для headless в `prepare_swarm_worker`.
3. **Инструменты только сужаются.** Эффективный allowlist равен пересечению `[tools]` и `profile.tools`, а `exclude_tools` добавляется к disabled. Профиль не может разрешить то, что запрещено глобально. Ограничение действует и для схем, и для вызовов, включая codemode и MCP.
4. **Порядок сборки статического промпта:** база (с фильтром по инструментам), модули возможностей, `# Agent profile: <name>`, self-dev, AGENTS.md, overlay, preferred-tools, скиллы (если `skills` не `false`). Всё это известно до первого запроса и замораживается вместе с префиксом. SDK `system_prompt` по-прежнему заменяет всё.
5. **Effort и модель:** сначала явный параметр spawn, затем поле профиля, затем `swarm_effort` и `swarm_model`. `profile.model` задан пользователем, поэтому считается доверенным наравне с `swarm_model`. Ограничения `swarm_pool` и `swarm_effort_caps` применяются после этого.
6. Встроенный профиль и пользовательский с тем же именем сливаются по полям: заданное пользователем поле побеждает.

Что остаётся на потом: смена профиля посреди сессии через `<system-reminder>`.

## Реализация P2 и P3

### P2. Промпты по семейству и по модели

Как у opencode (`optimize.ts`), но правила задаются в конфиге, а не в коде, поэтому новая модель из каталога попадает под правило без релиза.

```toml
[[agents.model_prompts]]
match = ["claude-*"]               # glob по ID модели, без учёта регистра
mode = "append"                    # append | replace
instructions = "..."              # или instructions_file = "~/.jcode/prompts/claude.md"

[[agents.model_prompts]]
match = ["gpt-6*-sol", "openrouter:*"]   # с двоеточием: provider:model
mode = "replace"
instructions_file = "~/.jcode/prompts/sol.md"
```

1. Правила применяются все, по порядку. `append` добавляется после базы, `replace` заменяет базу (`system_prompt.md` или пользовательский override). Из нескольких `replace` побеждает последний. Это позволяет общий текст для семейства и уточнение для конкретной модели.
2. В отличие от opencode, правило модели не отключается профилем агента. Профиль отвечает за роль, правило модели за особенности модели, и оба попадают в промпт. Порядок: база или замена, добавки модели, модули возможностей, профиль, self-dev, AGENTS.md, overlay, скиллы.
3. Метки `<!-- tools: ... -->` работают и в тексте правил.
4. Снимок промпта хранит ключ сработавших правил и модель. Смена модели и так начинает новый кэш провайдера, поэтому если при смене меняется набор правил, префикс собирается заново. Смена в пределах тех же правил сохраняет байты. Правка конфига сама по себе работающую сессию не меняет. SDK `system_prompt` заменяет всё, как и раньше.

Встроенных текстов по семействам нет: замеров, что они помогают нашим моделям, пока нет. Механизм есть, тексты пишет пользователь.

### P3. Advisor

Взято от Claude Code: модель сама решает, когда спросить (до выбора подхода, когда застряла, перед «готово»), через инструмент `advisor`. Взято от pi: advisor видит не историю целиком, а дайджест в пределах бюджета и отвечает вердиктом `NOTE`, `CONCERN` или `BLOCKER`. Не как в omp: никаких дельт транскрипта и выселения, один запрос без инструментов.

```toml
[agents.advisor]
model = "claude-opus-5-5"   # без model инструмента нет вовсе
effort = "high"             # по умолчанию high
max_digest_tokens = 12000   # по умолчанию 12000
auto_review = false         # ревью каждого хода с инструментами, как в pi
# instructions / instructions_file заменяют рубрику
```

- **Дайджест** (`crate::advisor::build_digest`): первый запрос пользователя без `<system-reminder>`, план и пункты todo, затем по порядку реплики пользователя, тексты агента и одна строка на вызов инструмента (команда, путь или запрос плюс intent). Результат инструмента сжимается до числа строк и 120 символов, ошибка до 400. При нехватке бюджета отбрасываются самые старые шаги с пометкой, сколько опущено. В тесте 5000 строк вывода сборки превращаются в одну строку, весь дайджест меньше 2 КБ.
- **Вызов:** форк провайдера на маршрут advisor через `fork_for_background_route`, effort через `set_reasoning_effort`, один `complete_simple_with_usage`, таймаут 300 с. Маршрут сессии не меняется. В ответ агенту приходит `[ADVISOR:<уровень>] совет` и число токенов.
- **Доступ:** инструмент регистрируется, только если задан `model`. Он есть в профилях `orchestrator` и `task`, в `sonic`, `scout` и `reviewer` его нет.
- **auto_review:** после хода, в котором были вызовы инструментов и не было вызова advisor, ревью идёт в фоне по дайджесту этого хода. `NOTE` только пишется в лог. `CONCERN` и `BLOCKER` приходят как завершённая фоновая задача с `wake`, поэтому агент реагирует, даже если ход уже закончился. Не больше трёх таких доставок на сессию за 30 минут, повтор того же совета отбрасывается. Ошибка advisor ход не ломает.

## Подсчёт токенов

Все оценки в jcode делят байты на 4 (`jcode_core::util::estimate_tokens`). Исключение: `compaction-core/shake.rs` делит символы на 4. Сравнение с токенизатором `o200k_base` (GPT) на наших текстах:

| Текст | байты/4 | o200k | Отношение |
| --- | ---: | ---: | ---: |
| `system_prompt.md` | 549 | 487 | 1,13 |
| Rust (`tool/todo.rs`) | 17 929 | 15 036 | 1,19 |
| Русский (этот документ) | 11 502 | 9 735 | 1,18 |
| Вывод инструментов (последние сессии) | 100 228 | 104 473 | 0,96 |

Для GPT оценка завышает на 13–19% и почти точна на выводе инструментов. У Claude токенизатор плотнее, там она ближе к точной или занижает. Решения о компакции берут максимум из оценки и числа input-токенов, которое вернул провайдер (`CompactionManager::update_observed_input_tokens_with_estimate`), а телеметрия этого аудита построена на числах провайдера. Поэтому менять подсчёт не нужно. Мелкая несогласованность: `shake.rs` на кириллице занижает примерно на 18% относительно байтов/4.

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
