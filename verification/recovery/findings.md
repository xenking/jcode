# Владение воркерами после reload

Проверено 16 ноября 2026 года. Цель проверки состоит в том, чтобы остановленный координатор не оставлял незаметно работающих воркеров.

## Что найдено

1. `ContinuationHasOwner` нарушается. Если воркер был running перед перезапуском и его история содержит признак прерванной работы, recovery запускает его даже при старом stopped-координаторе. Текущий reload-intent тоже обходит проверку владельца. Для последнего обхода уже существует положительный тест, поэтому это явно разрешённое поведение, расходящееся с проверяемым контрактом.
2. `StopAfterReadPreventsLaunch` нарушается. Recovery читает владельца, затем освобождает members lock. Координатора останавливают, каскадная отмена не находит ещё не созданного turn. Recovery использует сохранённое разрешение и запускает воркера после stop.

Оба пути воспроизведены в настоящем `Server::recover_headless_sessions_on_startup`, а не только в модели. Тесты `formal_recovery_running_worker_requires_owner` и `formal_recovery_stop_after_owner_read_prevents_continuation` завершаются соответствующим assertion failure. Они помечены ignored, потому что продуктовый код в этой задаче не исправляется. Таймаут или ошибка компиляции не считаются воспроизведением.

Это доказывает возможность продолжения генерации. Проверка не доказывает запись файла, не воспроизводит конкретный пользовательский reload и не проверяет оставшиеся дочерние процессы.

## Кратчайшие пути

TLC 1.7.4 через /statechart обошёл 86 состояний и 90 переходов. Это полный конечный граф этой модели, а не всех состояний Jcode.

Путь `ContinuationHasOwner` содержит пять шагов после начального состояния.

0. Воркер выбран для recovery.
1. Прочитан исторический владелец, не признанный живым.
2. Предыдущее running-состояние разрешает продолжение.
3. Захвачен turn воркера.
4. Воркер отмечен running.
5. Продолжение запущено без владельца.

Путь `StopAfterReadPreventsLaunch` содержит шесть шагов после начального состояния.

0. Воркер выбран для recovery.
1. Владелец признан живым на границе reload.
2. Координатор остановлен. Каскадная отмена до изменения статуса не находит turn.
3. Незавершённая работа разрешена по старому результату проверки.
4. Захвачен turn воркера.
5. Воркер отмечен running.
6. Продолжение запущено после stop.

На диаграмме `Worker starts without an owner` нарушает `ContinuationHasOwner`. Состояние `Worker starts after coordinator stop` нарушает оба свойства. Скрыты только 16 переходов, остающихся в том же отображаемом состоянии, включая терминальные self-loop.

## Проверка намеренной поломкой

В scratch-копии настоящего `server.rs` условие `live_before_restart.contains(&member.session_id)` заменено на `true`. Изменённая функция действительно скомпилирована, её результаты переданы модели. Дополнительно нарушилось `HistoricalEvidenceCannotAuthorize`. После экспорта нетронутого исходника это свойство снова выполняется, а две исходные находки остаются. Удаление проверки поймано по поведению, а не только по hash или diff файла.

Выполните `python3 verification/recovery/check.py --mutation-test`. Успешный exit 0 подтверждает чувствительность проверки. Обычный `python3 verification/recovery/check.py` возвращает exit 1, пока исходные находки остаются.

## Проверки и ограничения

- В модели один воркер, один найденный координатор, один restart и один stop между чтением владельца и решением о продолжении. Полные границы описаны в `README.md`.
- Историческая запись сама по себе не разрешает запуск. `HistoricalEvidenceCannotAuthorize`, `TypeOK` и отсутствие тупиков проходят на нетронутом коде.
- После rebase семь существующих startup recovery tests проходят, два новых ignored replay падают только на проверяемых assertions. Восемь Python-тестов проверяют извлечение исходника, независимый запуск свойств, обработку ошибок TLC и checksum инструмента.
- Обычный clippy проходит с исходными предупреждениями. После rebase strict clippy с `-D warnings` останавливается на `collapsible_if` в неизменённом `crates/jcode-provider-openrouter-runtime/src/lib.rs:515`. Предупреждения не подавлялись в продукте.
- Проверки test isolation, cell-width chrome и форматирования проходят. Живой пользовательский сервер не перезагружался.

Model the Domain определил выбор явных состояний владения и admission. Prove It Works потребовал реальных server-replay и поломки настоящего Rust guard вместо одной диаграммы.

## Диаграмма

```mermaid
---
title: "Worker recovery can outlive its coordinator"
---
stateDiagram-v2
  state "Legend" as legend {
    lg0: Breaks ContinuationHasOwner or StopAfterReadPreventsLaunch
    lg1: Path to the ContinuationHasOwner or StopAfterReadPreventsLaunch bug
  }
  class lg0 breaks
  class lg1 bugPath
  s0: Recovery candidate
  s1: Only historical ownership evidence
  s2: No coordinator found
  s3: Coordinator was live before reload
  s4: Continuation approved without an owner
  s5: Continuation approved
  s6: Coordinator stopped after the ownership check
  s7: Worker stays idle
  s8: Continuation approved after coordinator stop
  s9: Worker turn reserved without an owner
  s10: Worker turn reserved
  s11: Worker turn reserved after coordinator stop
  s12: Worker marked running without an owner
  s13: Worker marked running
  s14: Worker marked running after coordinator stop
  s15: Worker starts without an owner
  s16: Worker resumes for its coordinator
  s17: Worker starts after coordinator stop
  [*] --> s0
  s0 --> s1: recovery reads the coordinator
  s0 --> s2: recovery reads the coordinator
  s0 --> s3: recovery reads the coordinator
  s1 --> s7: recovery leaves the worker idle
  s1 --> s4: a current reload intent approves work or previously running work is approved
  s2 --> s7: recovery leaves the worker idle
  s2 --> s4: a current reload intent approves work or previously running work is approved
  s3 --> s5: unfinished work is approved or a current reload intent approves work or previously running work is approved
  s3 --> s6: the user stops the coordinator
  s4 --> s9: recovery reserves the worker turn
  s5 --> s10: recovery reserves the worker turn
  s6 --> s8: unfinished work is approved or a current reload intent approves work or previously running work is approved
  s8 --> s11: recovery reserves the worker turn
  s9 --> s12: the worker is marked running
  s10 --> s13: the worker is marked running
  s11 --> s14: the worker is marked running
  s12 --> s15: recovery starts the continuation
  s13 --> s16: recovery starts the continuation
  s14 --> s17: recovery starts the continuation
  classDef breaks fill:#ffecee,stroke:#e72e3b,stroke-width:2.5px,color:#cd2834
  class s0 bugPath
  class s1 bugPath
  class s3 bugPath
  class s4 bugPath
  class s6 bugPath
  class s8 bugPath
  class s9 bugPath
  class s11 bugPath
  class s12 bugPath
  class s14 bugPath
  class s15 breaks
  class s15 bugPath
  class s17 breaks
  class s17 bugPath
```
