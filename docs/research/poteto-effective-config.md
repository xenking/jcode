# Poteto-mode в Jcode и Jcode Desktop

Срез от 21 ноября 2026 года. **Ни в `jcode`, ни в `jcode-desktop` отдельная настройка poteto не нужна.** Эффективный skill общий. Desktop получает каталог и состояние сессии от того же daemon API. На обоих установленных клиентах ручное включение доставило инструкции координатору и headless-воркеру. Отдельная копия в repo перехватит имя и может разойтись с общей версией.

## Что проверялось

- Pstack `0.15.46`, upstream snapshot `8260de23` (`cursor/plugins/pstack@0b46ec3`), локальная ветка `jcode` и маркер установки `.pstack-upstream` на `86ef8d7b`. Сравнивались исходный `SKILL.md`, адаптированный checkout, установленный `SKILL.md`, `worker.md`, `docs/harness/jcode.md` и инструкция установки из `jcode/sync-pstack-skills`.
- Runtime: установленный CLI `v1.0.176-dev (2aaf1392e)`, Desktop `v0.7.0 (2ec815bd)`. Launcher `~/.local/bin/jcode` ведёт через `~/.jcode/builds/current/jcode` к `versions/2aaf1392e/jcode`. В момент проверки `builds/shared-server/jcode` указывал на тот же артефакт. В пробе shared daemon не участвовал. SHA-256 CLI `a9db289c7ea9c541dd44dccb58988ec8c90ed419a25653264f03975831a1650d`, Desktop `7209dfabc325b824e7198fa39b095e5cbc60d4c20d5644b8ab41bf8b00319c39`.
- Исходные базы аудита: fork `jcode` `7ccf0906d`, fork Desktop `1f411bda`. Точки Desktop ниже сверены и на установленном `2ec815bd`. Desktop запускался в невидимом native-окне с той же установленной версией CLI в роли private daemon и API bridge. Не заменялись приложения, daemon, пользовательские настройки и файлы инструкций.
- Наблюдение в runtime означает присутствие в реальном HTTP-запросе к локальному провайдеру. Оно не означает, что модель прочитала инструкцию, поняла её или выполнила. Отдельную проверку реальных задач см. ниже.

## Эффективная конфигурация

| Тема | Задумано | Настроено | Наблюдали | Пробел |
| --- | --- | --- | --- | --- |
| Источник и версии | Pstack описывает workflow, роли, контрольные шаги и обязательную первую строку kickoff о `worker.md`. Jcode-карта адаптирует Cursor и утверждает, что установленные skills являются symlink. | `~/.jcode/skills/poteto-mode` и `SKILL.md` не symlink. Установщик копирует с маркером, добавляет banner с абсолютным путём к карте. Хеш установленного `SKILL.md` начинается `a31a49c0`, checkout `851b917a`. `worker.md` побайтно совпал (`d0510605`). В checkout карта есть, в upstream snapshot её нет. | Между checkout и установленным `SKILL.md` только добавленный banner. Ни в одном repo нет локального одноимённого skill в `.jcode/.agents/.claude/skills`. | Документация symlink устарела. Три указания про «первое» расходятся: banner просит карту, карта для `poteto-agent` просит `SKILL.md`, kickoff-контракт требует `worker.md`. |
| Разрешение путей | В карте заявлены global и project skills. | Приоритет от сильного к слабому: `<cwd>/.jcode/skills` → `<cwd>/.agents/skills` → `<cwd>/.claude/skills` → `$JCODE_HOME/skills` (обычно `~/.jcode/skills`) → `~/.agents/skills` → установленные Claude plugins. Имя из frontmatter, а не имя каталога, определяет коллизию. `~/.claude/skills` и `~/.codex/skills` могут импортироваться при первом создании Jcode skills, но не являются постоянными отдельными слоями. `~/.jcode/pstack`, `.cursor/skills` и прямой обход предков не являются источниками registry. | Runtime выбрал project `.jcode`, затем `.agents` после удаления первого источника, затем global после удаления project overlay. | Остальные precedence-ступени подтверждены кодом, а не отдельным запуском каждой комбинации. Под `JCODE_HOME` `storage::user_home_path` разрешает внешние файлы в `$JCODE_HOME/external/`, даже при другом `HOME`. |
| Ручной запуск и автозагрузка | Оператор включает режим, тело `SKILL.md` действует на сессию. Pstack использует метаданные Cursor `disable-model-invocation: true`. | Новый runtime видит описания skills, но не включает режим сам. TUI `/poteto-mode` и `/mode poteto-mode`; `/mode toggle …` добавляет/убирает одну позицию, `/mode off` сбрасывает. `skill_manage read/load` возвращает текст как результат tool, не меняет pinned roster. В Rust frontmatter `disable-model-invocation` не реализован. | Свежий TUI и Desktop получили каталог без тела. Bare slash TUI не вызвал провайдера, следующий запрос получил тело. Desktop native `/mode` picker обновил roster, следующий запрос получил тело. | Не путать появление skill в каталоге, tool load, pinned mode и подтверждённое соблюдение. Отсутствие автоактивации по имени не означает запрета вызывать `skill_manage`. |
| Desktop против TUI/CLI | Один session state, разные interface paths. | Desktop UI классифицирует mode по `mode` или именам `selfdev`, `swarm`, суффиксу `-mode`. У poteto нет собственного `mode: true`, есть `icon: crown` и подходящий суффикс. Runtime `/mode` проходит через SDK и daemon; Desktop skill slash с текстом зависит от каталога и идёт через `SendSkillMessage` или очередь с `active_skill`. Bare skill slash в Desktop может остаться локальным выбором до отправки prompt; `/mode` отправляет команду сразу. `jcode run` не является TUI slash-парсером. | Desktop проверяли через fixture-платформенный Enter, не через `desktop.send`: его `control_submit` намеренно посылает буквальный текст. Реальный private bridge применил `/mode`. | Не проверяли все способы выбора slash в Desktop, подготовку draft, remote SSH backend и режимы старого bridge. Потеря slash при `desktop.send` не является багом picker. |
| Headless | Режим и рабочие инструкции доступны воркеру; kickoff отдельно требует прочитать `worker.md`. | При spawn имена приходят из сохранённой родительской сессии. Для тела приоритет worker overlay/global, затем parent overlay, затем сохранённое объявление. Воркер строит свои `AGENTS.md` из собственного `working_dir` и общего home, а не копирует родительский снимок. Routing overrides и session env наследуются отдельными путями. Kickoff/playbooks/role file не превращаются в автоматически читаемые файлы. | В первом HTTP-запросе headless от обоих клиентов есть pinned body, project и global AGENTS. Маркер дочернего `worker.md` отсутствует. При пропаже источника имя осталось в сохранённых метаданных воркера, но в его первом HTTP-запросе не было тела. Приватная исправленная сборка передала сохранённое тело в том же сценарии. | Разный cwd и смена одноимённого определения не тестировались в native Desktop. Наследование тела из registry не гарантирует его побайтное равенство родительскому экземпляру. |
| Reload / compaction | Активный режим должен пережить границу истории и восстановление, не вычитаться из summary и не дублироваться. | Имена в `pinned_skills` и legacy `pinned_skill`. Объявление записано как скрытый `display_role=system` с provider `role=user`. Restore сравнивает актуальное объявление с историей после границы, при необходимости повторяет. Статический bootstrap (`AGENTS.md`, каталог навыков в system prompt) заморожен для восстановленной сессии с тем же cwd. При новом cwd или новой сессии строится заново. Project registry перечитывается отдельно, global registry обновляется через `skill_manage reload[_all]` или новый процесс, не равнозначно переключению или переписыванию старых сообщений. | Установленный `mode-set` сохранил два режима при restart, TUI `/compact` восстановил тело вместе с summary в следующем запросе. `worker-reload` проверил restart в idle и настоящий exec reload во время выполнения: 10 подходящих HTTP-запросов к fake provider содержали pinned body. | Нет основания считать, что произвольное изменение активного файла немедленно перепишет контекст уже идущего turn. Переходы жизненного цикла проверены через TUI на общем daemon. Отдельное переподключение Desktop и другие провайдеры не проверялись. |
| Роли, модель и effort | Pstack role settings определяют подходящую модель и effort; Cursor использует свои fast-слуги и per-task effort. Jcode-карта заменяет модель на точный route, effort задаётся отдельно. | `~/.agents/pstack-models.md`: `# defaults: 2`, budget `large (xhigh)`, feature/bug-fix/refactor/perf/hillclimb `openai:gpt-6.1-sol xhigh`, `swarm workers` `openai:gpt-6.1-terra high`, judgment `inherit-parent`. Это **инструкция для агента**, не runtime config. `[agents]`: model selection включён; обе явные allowlist содержат DeepSeek flash/pro, Terra и `openai:gpt-6.2-sol`, дополнены routes из `swarm_pool` (Sonnet, Haiku, Opus 5.5 и GPT-6-luna). `6.1-sol` нет в обеих итоговых allowlist; его cap `xhigh` не даёт допуска. `6.2-sol` разрешён, cap `high`. `swarm_effort=high`, `swarm_spawn_mode=inline`, `swarm_root_effort` не задан. Default worker `opencode-go:deepseek-v4.1-flash`, fallbacks `opencode-go:deepseek-v4-pro`, `openai:gpt-6.2-sol`. | По коду неуказанный `model` выбирает session override или настроенный runtime default, а не роль `inherit-parent`. Реальный runtime sentinel `inherit` / `coordinator` отличается от строки role file и не является способом обойти allowlist инструмента. В частном стенде координатор и воркер вышли на `fake-model`; compatible adapter не подтвердил применение vendor effort (`reasoning_effort` отсутствует). | Не проверяли реальное наличие подписок, выбор моделей на живом аккаунте и какой effort дошёл бы до vendor API. Модель из role file сама собой не выбирается. Название swarm fallback в role settings не равно порядку failover в daemon. |

### Точный дефект и причины

И headless/inline (`crates/jcode-app-core/src/server/headless.rs`), и подготовка видимого воркера (`server/comm_session.rs`) берут `Session::load_startup_stub`. В stub намеренно нет `messages`. Поэтому fallback `stored_pinned_skill_announcement()` не находил тело при недоступности всех источников, хотя режим оставался pinned. `Session::inherit_pinned_skill` теперь читает сохранённый transcript только в этом редком случае. Обычный spawn без полной загрузки, выбор источника, defaults и глобальные настройки не менялись.

Красный тест использовал уникальный второй kickoff, чтобы результат первого воркера не мог замаскировать ошибку. После удаления только временного `SKILL.md` координатор имел тело в transcript, новый worker сделал запрос без него. Та же проверка стала зелёной на приватной сборке с фиксом. Не считать сам факт сохранения `pinned_skills` доказательством доставки тела.

## История локальных задач

Окно UTC **[2026-11-14 08:49:54, 2026-11-21 08:49:54)**. Единица наблюдения: первая сохранившаяся пользовательская задача или kickoff каждой сессии, созданной внутри окна. Не все реплики и не все задачи по проекту. Сравнивались точные canonical checkout roots двух repo, а не пути других проектов, их дочерние каталоги или отдельные worktrees. Проверены metadata 611 snapshots; содержимое сообщений и append-journal классифицировались только для сессий этих двух repo. Повторы по message ID устранены, тестовые флаги исключаются. Нечитаемых или нестабильных snapshots в проходе не наблюдалось. Восьми подходящим сессиям не хватало видимой первой задачи.

| Независимый признак | jcode | jcode-desktop |
| --- | ---: | ---: |
| Сессий в окне / с наблюдаемой первой задачей | 186 / 178 | 18 / 18 |
| Тип среди задач: worker / root или без надёжной маркировки | 118 / 60 | 16 / 2 |
| `poteto-mode` явно объявлен перед задачей / явно off или иной roster / неизвестно | 54 / 8 / 116 | 7 / 0 / 11 |
| Наблюдалась успешная выдача текста `SKILL.md` или `worker.md` через tool / неизвестно | 58 / 120 | 9 / 9 |
| Прямые чтения по видам: `SKILL.md` / `worker.md` / карта Jcode / playbook | 8 / 50 / 36 / 0 | 3 / 6 / 6 / 1 |
| Релевантный проверяемый worker-flow: `worker.md` первым действием / явных kickoff | 24 / 24 | 6 / 6 |
| Другой flow или его применимость не установлены | 154 | 12 |

Для объявления нужен соответствующий системный элемент истории до первого задания. Снимок последнего roster не подставлялся за историческое состояние. Для «чтения» нужна пара прямого tool call и успешного ответа с заголовком документа. Это доказательство доставки текста через tool, не чтения глазами модели. Видовые счётчики могут пересекаться. Для flow применялся узкий критерий: явный kickoff `Read …/poteto-mode/worker.md before anything else.` и первый зафиксированный tool call с соответствующим успешным ответом. Прямых координационных `swarm spawn` с доказанной применимостью в **первых** задачах не обнаружено. Нельзя из этого заключить, что launch уже достаточен или что любой bug-fix прошёл red → green.

Не видны удалённые и не сохранённые сессии, часть истории в других worktrees/машинах, исключённые старые сессии с работой внутри окна, вложенные чтения и spawn внутри Codemode, действия вне harness, скрытые рассуждения, действия после следующего запроса. Репозиторий сессии не идентифицирует интерфейс, через который её открыли. Неизвестное не считалось нарушением, а полученные доли не сравнивают дисциплину TUI и Desktop. Никакие ID, тексты пользовательских задач, tool outputs, учётные данные или реальные транскрипты не выводятся и не отправляются наружу.

## Рекомендации и места правок

1. **Не заводить конфигурацию poteto в обоих repo.** В `jcode` обнаружен один дефект общего session runtime, исправленный в `crates/jcode-base/src/session/pinned_skill.rs`. В Desktop отдельного дефекта проводки mode не подтверждено. Его проверенная цепочка проходит через `panel_mode.rs`/`panel_runtime_commands.rs`, `jcode-desktop-harness/src/harness.rs::load_skill_catalog` и общий daemon. Repo-специфический `AGENTS.md` нужен для правил разработки этого repo, не для переопределения режима, модели, effort или автоматической активации.
2. **Исправить источник глобальной документации, не её копии.** В pstack `docs/harness/jcode.md`, вводный абзац и таблицы `Subagent fields`/`Models`/`Paths`: задокументировать `.pstack-upstream` и копирование из `jcode/sync-pstack-skills`, согласовать kickoff с `worker.md` первым, далее harness и при необходимости полный `SKILL.md`. Указать реальный приоритет dirs, `apply_swarm_pool` и то, что omit-model не означает наследование родителя при настроенном `swarm_model`. Не переносить эти правила в `AGENTS.md`.
3. **Синхронизировать global роли только по решению владельца.** Обновить `~/.agents/pstack-models.md` до схемы `defaults: 3` (`/setup-pstack`) и выбрать маршруты из эффективной policy в `~/.jcode/config.toml` `[agents]`. Если цель реально требует `xhigh`, сначала согласовать модель, разрешения и cap. Не менять defaults/caps только ради совпадения со старой строкой роли. Если нужен fail-fast при рассинхронизации, добавить проверку в pstack setup/sync, а не новый runtime default в jcode или Desktop.
4. **Если нужен строгий запрет model invocation, определить его отдельно.** Место реализации в `crates/jcode-base/src/skill.rs` (frontmatter) и `crates/jcode-app-core/src/tool/skill.rs` (доступ модели). Для этого нужен явный контракт со всеми harness и совместимость со skills, которые другие playbook читают через tools. Это не исправлялось как часть диагностической задачи.

## Повторение

Нужны Python 3.10+ и tmux. Desktop требует macOS и установленный или предварительно проверенный бинарник со скрытым control fixture (`src/main.rs`, оба `JCODE_DESKTOP_SCREENSHOT` и `JCODE_DESKTOP_CONTROL_TEST`, `show/focus` выключены). Переданный `--jcode` используется только для приватной копии, `--desktop` только для запуска отдельного окна. `--skill-file` необязателен; без него используется тестовый `SKILL.md`. Папки evidence не должны существовать. В них только synthetic fixtures и их transcript, не реальные сессии. Не добавлять evidence в git.

```sh
BIN="$HOME/.jcode/builds/versions/2aaf1392e/jcode"
SKILL="$HOME/.jcode/skills/poteto-mode/SKILL.md"
E="${JCODE_SCRATCH_DIR:-/tmp}/poteto-$(date -u +%Y%m%dT%H%M%SZ)-$$"

# Красный до фикса именно на unavailable_source_fallback.
python3 scripts/probe_poteto_mode.py --jcode "$BIN" --skill-file "$SKILL" --evidence "$E-red"

# Тот же стенд, native Desktop против private API bridge.
python3 scripts/probe_poteto_mode.py --jcode "$BIN" --skill-file "$SKILL" --evidence "$E-desktop" \
  --desktop /Applications/Jcode.app/Contents/MacOS/jcode-desktop

# Зелёная проверка требует бинарник с фиксом. Установку не заменять.
TARGET="$PWD/target/poteto-probe"
justrust build --local='run a private loopback acceptance' --target-dir "$TARGET" \
  --profile selfdev -p jcode --bin jcode
python3 scripts/probe_poteto_mode.py --jcode "$TARGET/selfdev/jcode" --skill-file "$SKILL" --evidence "$E-green"

# Дополнительные lifecycle-проверки общего backend на установленном TUI.
V=.jcode/skills/verify-jcode/scripts/verify-jcode
(
  set -e
  trap '"$V" cleanup --evidence "$E-lifecycle"' EXIT
  "$V" launch --binary "$BIN" --evidence "$E-lifecycle"
  "$V" drive --evidence "$E-lifecycle" mode-set
  "$V" drive --evidence "$E-lifecycle" mode
)
(
  set -e
  trap '"$V" cleanup --evidence "$E-exec"' EXIT
  "$V" launch --binary "$BIN" --evidence "$E-exec"
  "$V" drive --evidence "$E-exec" input --text '/mode verify-probe' --expect 'Session mode: verify-probe'
  "$V" drive --evidence "$E-exec" worker-reload
)

# Только локальные агрегаты. Не запускать на удалённом builder/CI.
python3 scripts/sample_poteto_sessions.py --sessions "$HOME/.jcode/sessions" \
  --repo "$PWD" --repo "$PWD/../jcode-desktop" \
  --start 2026-11-14T08:49:54Z --end 2026-11-21T08:49:54Z
```

Для выборки `--repo` должны указывать на реальные checkout из истории, не на временное worktree отчёта. Probe применяет чистые env, `HOME`, `JCODE_HOME`, Unix sockets, `JCODE_TEST_SESSION=1`, update/telemetry opt-outs, фиктивный ключ и `127.0.0.1`. Это не OS-level сетевой sandbox. Проверяются только запросы к fake inference boundary, не политика vendor API или verbatim snapshot облачной истории.
