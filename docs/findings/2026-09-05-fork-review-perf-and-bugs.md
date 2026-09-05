# Ревью дельты форка (v1.7.2..HEAD): баги и производительность

Дата: 2026-09-05. Диапазон: `aa8ac4b04e261f19c2465f68e9ce2fa9721ae1a2..HEAD` (966 коммитов,
682 файла, +248k/−16k строк). Метод: 17 поисковых суб-агентов по срезам (git-панель, diff/blame,
solutions, граф/репозиторий, консоль/агент, solution_agent store и db/mcp/view, платформа/CLI,
удалённое upstream-поведение, эффективность, переиспользование, упрощение, «высота» абстракций,
конвенции), затем ручная верификация каждой находки по текущему дереву. Всё ниже подтверждено
чтением кода; строки указаны по HEAD `603d46965c`.

Статус: **исправлено в тот же день** (11 групп правок, каждая с тестами; см. §0). Разделы 1–2 ниже
описывают дефекты *как они были найдены* — они оставлены как есть, потому что «почему так было»
дороже «что стало», а состояние кода читается из кода. Что осталось — в §6.

---

## 0. Что сделано 2026-09-05

Правки раздавались 11 группам по непересекающимся наборам файлов, каждая со своими тестами.
FORK.md: новые решения #150–#155, поправки к #114 (hand-off несёт `--solution`), #115 (Install CLI
перелинковывает свою же ссылку), #116 (`rescan_branches` инвалидирует только при смене списка веток),
#140 (`BlameRunPredecessor::Unsettled`), плюс семь строк в таблицу тронутых upstream-файлов.

Гейты после всех правок: `cargo check --workspace --all-targets` — 0 ошибок, 0 предупреждений;
`cargo fmt --all --check` чисто; `./script/clippy` (весь workspace, release, `--deny warnings`) —
зелёный; `cargo test` по 15 затронутым крейтам — **2491 passed, 0 failed** (+ `claude_native`
90 + 16 отдельно, см. ниже). Живая проверка в headless-редакторе со скриншотами: «Open in Terminal»
из контекстного меню папки, авто-старт шелла для второго участника, тост о неудачном шелле, клик по
пункту подменю поповера.

Попутно закрыты пять нарушений линтера, существовавших до этой сессии (файлы мы не меняли):
`denoise/src/lib.rs` (`while_let_loop`), два `needless_lifetimes` в тестовых хелперах
`solutions_ui`, `Iterator::last` в `commit_view/affected_files.rs`, лишний `into_iter()` в
`stashes.rs` и лишний clone в `conflict_view.rs`. Гейт проекта был красным и до правок.

`claude_native::process::tests::dropping_the_process_kills_the_child` — предсуществующий флейк
(файл не менялся): тест ждёт реаппинга убитого потомка максимум 2 секунды и не выдерживает
параллельного прогона 15 крейтов; в изоляции проходит 5/5, полный крейт — 2/2.

Три вещи оказались глубже, чем описано в отчёте, и это стоит помнить:

1. **§1.1 назвал не ту причину.** Зарегистрировать `OpenTerminal` в `console_panel` недостаточно:
   оба обработчика висят на одном узле диспатча `Workspace`, bubble-фаза останавливается на первом
   не пропускающем слушателе, а `terminal_view::init` выполняется раньше `console_panel::init`. Тем
   же порядком был убит и `ctrl-~` (`workspace::NewTerminal`) — в отчёте этого не было. Оба
   обработчика `TerminalPanel` теперь делают `cx.propagate()`, когда панели нет (решение #151).
2. **`mod_seq` — не версия содержимого.** Очевидный ключ кэша `(index, mod_seq)` для §2.2 ломается:
   зачистка зависших tool call'ов переписывает статус через `Arc::make_mut` без бампа, и карточка
   отменённого вызова навсегда осталась бы «running» — тот же баг с вечным спиннером, только
   переехавший во вью. Решение #153.
3. **§1.13 про поповер был неточен.** Обёрточный `on_mouse_down_out` — проверка границ, и починить
   её расширением на подменю `ContextMenu` нельзя: любое содержимое, рисующее себя вне обёртки,
   ломается так же. Заменено на невыключающую подложку внутри того же deferred-поддерева, где
   порядок hit-теста и отвечает на вопрос «попали ли мы в поповер» (решение #150).

---

## 1. Баги — высокий приоритет

### 1.1 «Open in Terminal» не работает нигде
`crates/terminal_view/src/terminal_panel.rs:526`. Единственный обработчик `workspace::OpenTerminal`
— `TerminalPanel::open_terminal`, который выходит, если в workspace нет `TerminalPanel`. Форк удалил
`TerminalPanel::load` + `add_panel_when_ready` из `zed.rs`, а `console_panel::init` регистрирует
только `NewTerminal`. Диспетчеры живы: project_panel:3626, pane.rs:3357 («Open in Terminal» в меню
вкладки), editor.rs:5822 (`OpenInTerminal`), header.rs:1006, outline_panel:2032. Итог: пункт меню и
экшен — молчаливый no-op. Та же причина: JetBrains-кеймапы (`alt-f12` → `terminal_panel::Toggle`,
linux/jetbrains.json:147, macos:150) и docs/src/terminal.md ссылаются на мёртвый экшен;
контекстное меню терминала гейтит «Inline Assist» на `panel::<TerminalPanel>().assistant_enabled()`
(terminal_view.rs:506), а флаг переехал в ConsolePanel — пункт не показывается никогда.
**Фикс:** зарегистрировать `OpenTerminal` в console_panel (открыть таб с cwd в utility-половине),
перепривязать JetBrains-кеймапы, перевести гейт terminal_view на ConsolePanel.

### 1.2 Гонка в добавлении участника Solution уничтожает чужой checkout
`crates/solutions/src/add_member.rs:167-174, 288`. Имя папки уникализируется против `sol.members`
в момент спавна, in-flight добавления не учитываются. Два добавления, чьи имена derive'ятся в одну
папку («Update Deps» и «Update-Deps»), получают один `target`; второе под `fs_lock` видит
`target.exists()` и делает `remove_dir_all` уже склонированного checkout первого, затем пушит второй
member с тем же `local_path`. Второй вектор: успешная ветка задачи не проверяет `cancel_flag` —
отмена во время `set_remote_url`/`checkout` всё равно пушит member; повторный add из пикера снова
сносит папку. Третий: дедуп `eq_ignore_ascii_case`, а `rename.rs::same_folder_name` — полный
Unicode-lowercase: на case-insensitive FS не-ASCII имена, отличающиеся регистром, приводят к тому же
`remove_dir_all` живого участника.
**Фикс:** включать `in_flight_adds` (их target-папки) в `taken`; резервировать папку под
`fs_lock`; проверять `cancel_flag`/наличие in-flight записи в Ok-ветке; один предикат равенства имён.

### 1.3 «Mark Unresolved» тихо теряет входящую сторону merge
`crates/git_ui/src/git_panel.rs:240`, changes_list.rs:832. Для конфликтной строки после
«Mark Resolved» пункт/тултип обещают «Mark Unresolved», но диспатчат `ToggleStaged` →
`git reset -- path`: в индекс ложится версия HEAD, unmerged-стадии не восстанавливаются, файл
выпадает из `ls-files -u`, а `Continue`/commit фиксирует HEAD-версию. Правильное действие —
`git checkout -m -- path` (восстанавливает конфликт), либо не предлагать обратный переход.

### 1.4 После /compact и /clear мёртвые фоновые тиммейты остаются «живыми» до часа
`crates/solution_agent/src/store.rs:3326` + `model.rs:757`. `rotate_context`/`reset_context`
вызывают `set_acp_thread(Some(new))`, а `mark_background_agents_killed` срабатывает только при
`thread.is_none()`. Старый subprocess закрывается — его async `Agent`-дети умирают без
`stop_reason`. Последствия: `has_live_background_work` = true → супервизор пропускает тики
(`supervisor_engine.rs:2116`), stuck-watchdog экранирован, пилюля тиммейта рисует Live до
3600-секундного бэкстопа. Это та же семья, что «Teammate tab lingering» и «Subagent usage-limit
spins forever». **Фикс:** `mark_background_agents_killed()` при любой замене потока, не только при
`None`.

### 1.5 «Lost-Stopped recovery» гасит уже начавшийся следующий ход
`crates/solution_agent/src/store/queue.rs:848`. Ветка Ok у send-задачи не проверяет идентичность
хода: `Stopped` ход 1 → `send_message_blocks` синхронно ставит `Running` для хода 2 (queue.rs:749)
→ future хода 1 резолвится → `still_running` истинно → `mutate_state(Idle)` на живом ходе 2
(GC стримов, state-changed на мобилу, следующая отправка идёт мимо очереди и отменяет ход 2).
**Фикс:** снимать turn-id/`started_at` при спавне и сравнивать перед force-flip.

### 1.6 Дубль ответа ассистента при чередовании с субагентом
`crates/claude_native/src/connection.rs:1647, 1891`. `text_streamed_for_current_message` /
`current_message_text` — одно общее состояние, которое сбрасывает `message_start` любого потока,
включая события с `parent_tool_use_id`. Async Agent, вклинившийся между дельтами Main и финальным
`assistant`, обнуляет буфер → `final_text_block_suffix` возвращает весь текст → второй
`AgentMessageChunk`. **Фикс:** трекать по ключу `(parent_tool_use_id)` или игнорировать
сброс для не-Main событий.

### 1.7 Stop при usage-wall снимает парковку и запускает judge в стену
`crates/solution_agent/src/store/acp_event.rs:193`. `clear_resume_gate_on_agent_response`
вызывается безусловно на любом `Stopped`, хотя ниже та же ветка говорит, что `Cancelled`
«ничего не доказывает о стене». Ручной Stop после парковки сбрасывает `next_eligible_ms` →
следующий тик спавнит judge в активный лимит. Гейт должен сниматься только когда
`response_proves_wall_lifted`.

### 1.8 Гонка загрузок в SoloDiffView
`crates/git_ui/src/solo_diff_view.rs:565` (и `open_or_focus:472`). Нет generation-guard'а: быстрые
стрелки по файлам коммита — побеждает последняя завершившаяся загрузка, а не последний выбор;
`resolve_gesture` решает Retarget до await, так что закрытая во время загрузки вкладка
воскресает. Также `DiffSource::matches` для `Commit` игнорирует репозиторий (:214) — два клона
одного проекта в Solution активируют чужую вкладку с чужим remote/blame; `can_split` + общий
`MultiBuffer` в `clone_on_split` (:1077) — split/unsplit мутируют общее состояние (в тестах уже
отмечено срабатывание debug-assert в display_map, TODO C6).

### 1.9 Commit-таб: перехват фоновой выборкой, закрытие по фильтру, потерянный UserGesture
- `commit_tab.rs:1279` — `Background`-push любого графа заменяет содержимое открытого таба
  (гейт только подавляет активацию); рефетч/`HeadChanged` из другого члена Solution или
  file-history pane подменяет коммит, который читает пользователь; мультивыбор `[a,b,c]`
  схлопывается в `[a]`.
- `git_graph.rs:2212` — `close_vanished_commit_tab` срабатывает и при рефетче по search/branch/path
  фильтру: отфильтровал коммит — таб закрылся, снятие фильтра не возвращает.
- `git_graph.rs:2899` — `select_commit_by_sha(.., UserGesture)` для ещё не загруженного коммита
  паркует голый sha и переигрывается как `Background` → `OpenAtCommit` подсвечивает строку, но таб
  не открывает (гейт `Background && !open`).
- `commit_tab.rs:1259` — `already_showing` сравнивает только repo+shas: re-push с новыми `refs`
  после создания тега/fetch отбрасывается, чипы и строки tags/branches остаются старыми.
- `commit_tab.rs:1347` — stale-guard по sha без репозитория (два клона → чужой результат).
- `git_panel.rs:4218` — `set_active_repository` закрывает таб и эмитит `CommitTabClosed`, по
  которому все графы с совпадающим выбором сбрасывают выделение; транзиентный `None` от
  `active_member_repository` во время rescan снимает выделение с непричастного pane-графа.

### 1.10 CLI hand-off: тихая потеря `--solution`, зависание CLI, ложные отчёты
`crates/zed/src/main.rs`. `--solution`, `--dev-container`, `--wsl` не переносятся в hand-off и не
считаются в loss-report (:713) — `sawe --solution x` при запущенном редакторе выходит с 0 и ничего
не делает. `sawe-cli://<server>` откладывается в `cli_handshakes`, hand-off отдаёт пустой список и
процесс выходит (:248, :733) — CLI ждёт `accept()` вечно (Dev/`ZED_STATELESS`). На Windows
`handle_single_instance` уже переслал пути по pipe, а затем печатается «NOT opened» (:817);
таймаут ответа >30s тоже трактуется как «не открыто» (:764) → повторный запуск даёт второе окно;
`--diff` считается парами в одном отчёте и токенами в другом (:258 vs :713).

### 1.11 Install CLI отказывается перезаписать собственную ссылку
`crates/install_cli/src/install_cli_binary.rs:56`. Любой symlink, чей target не байт-в-байт равен
текущему `cli_path` (перенос бандла, другой канал, апгрейд по новому пути), — `Foreign` → bail с
сообщением «Sawe only replaces a symlink it created itself». Раньше перелинковывалось. Нужно
распознавать «наш» target (тот же bundle-id / basename `cli` внутри `*.app`/`sawe*`).

### 1.12 Linked worktree / submodule: нет баннера «merge in progress»
`crates/git_ui/src/git_panel.rs:4318`. `detect_in_progress_op(&repo.dot_git_abs_path)` — для
linked worktree это gitfile, `MERGE_HEAD` там нет. Нужен `repository_dir_abs_path` (так делает
`git_conflict_ui::op_for_dir`).

### 1.13 Кросс-крейтовые регрессии в общей инфраструктуре
- `crates/ui/src/components/popover_menu.rs:395` — общий для всех `PopoverMenu`
  `on_mouse_down_out` на обёртке меню. Сабменю `ContextMenu` рисуются `absolute().left_full()`
  ВНЕ bounds обёртки (context_menu.rs:1694), а собственный обработчик ContextMenu это исключает
  через `padded_submenu_bounds` (context_menu.rs:2189) — обёртка нет. Mouse-down на пункте сабменю
  должен закрывать весь поповер до click'а. Затронуты: title bar user-menu «Panel Layout»
  (title_bar.rs:889), edit-prediction «Experiment», сабменю полосы AI-сессий
  (session_tab_strip.rs:794). **Нужна живая проверка** (по коду — воспроизводится).
- `crates/project/src/git_store.rs:10648` — `compute_snapshot` заменил три независимо
  дефолтящихся future (`log_err().unwrap_or_default()`) одним `try_join4` с новым `tag_names()`,
  который делает `ensure!(status.success())`. Падение любого подзапроса (`for-each-ref refs/tags`,
  `worktree list`, `show(head)`) обнуляет branch, branch_list, head_commit, worktrees и tags разом и
  публикует это как изменившийся snapshot: пустой пикер веток, пропавшие ref-чипы.
  **Фикс:** `join4` + per-field `log_err().unwrap_or_default()` как в upstream.
- `crates/workspace/src/dock.rs:1050` — `resize_all_panels` и `resize_active_panel` делят один
  `_persist_panel_size_task`; settings-driven запись N панелей отменяется drag'ом активной панели
  в пределах 200 ms, и любая незавершённая запись теряется при drop Dock (закрытие окна).
  Upstream писал синхронно через `cx.defer`.
- `crates/gpui/src/elements/list.rs:705, 1352` — `ListState` теперь ре-армит tail-follow только
  после wheel-`scroll()` (`reengage_check_pending`); `scroll_to_end()`/`scroll_by()` больше не
  восстанавливают «прилипание к низу». `solution_agent` обновлён (`set_follow_mode(Tail)`,
  session_view.rs:805), `agent_ui/thread_view.rs:1615/6158/6684` — нет. В форке AgentPanel не
  монтируется (zed.rs:508), так что латентно, но контракт изменён молча — любой другой потребитель
  `ListState` в upstream-коде получит ту же регрессию при merge.

### 1.14 Прочее (средний приоритет)
- `store/hydration.rs:1509` — ошибка `list_open_tabs` глотается `unwrap_or_default` → все сессии
  с `tab_order = None`, пустая полоса вкладок, ни строчки в логе.
- `upload.rs:113` — нет серверного лимита `total_size`; `resolve_upload_handles_with` читает файл
  целиком и base64-ит в памяти на foreground — клиент с багом заполняет диск/кучу.
- `store.rs:2590` — `pending_stop` растёт на каждый SubagentStop без регистрации (inline Task /
  sync Agent) до сброса контекста; ограниченная, но утечка.
- `console_panel/panel.rs:987` — авто-старт шелла ключуется на store-строку Solution: retained
  фоновый workspace той же Solution тоже спавнит PTY. `:1125` — проверка `tabs.is_empty()`
  глобальная, а рендер per-member: у члена B половина пуста, а авто-старт не срабатывает.
  `:1150` — провал спавна только в лог, пустая половина без тоста.
- `reopen_session_modal.rs:73` — двойной клик по кнопке → второй `toggle_modal` закрывает
  только что открытый пикер.
- `session_view.rs:1427` — drag ручки compose считает от модельной `compose_height`, а блок теперь
  сжимаем flex'ом: мёртвая зона и «фантомный» рост, выстреливающий при расширении band.
- `git_conflict_ui/src/operations.rs:227` — `parse_porcelain_z` съедает `origPath` только для
  R/C в index-колонке; ` R`/` C` в worktree-колонке парсятся как запись → ложный «staged» путь
  блокирует Continue.
- `commit_view.rs:285` — `CommitView::open` дедупит по `commit.sha` и вытесняет открытую
  Compare-Versions `A..B` вкладку, чей head = B.
- `branch_submenu.rs:430` — `upstream.gone` гасит только Delete; «Checkout and Update» и Tracked
  Branch → Checkout для `[gone]` остаются активны и падают с ошибкой git.
- `git_graph.rs:2950` — двойной клик суммонит панель независимо от модификаторов: ctrl+dblclick
  включает/выключает строку и открывает панель на пустом выборе.
- `git_graph_panel.rs:202` — удерживаемый граф прошлого проекта остаётся интерактивным: клик во
  время hold пушит коммит СТАРОГО репозитория в панель нового; promote ничего не закрывает.
- `log_toolbar/*_popover.rs:446/495/529` — `cursor.move_to(render-time ix)` при асинхронной
  перестройке строк ставит курсор на другую ветку.
- `commit_tab.rs:2189` — `ref_row_fit` бюджетирует по ширине, измеренной только когда строка
  рефов рисовалась: после ресайза на недекорированном коммите — кадр с обрезанными чипами.
- `blame_ui.rs:41` — резерв ширины gutter'а предполагает колонку ≥ 8px; при buffer font 12 или
  ui font 20 самый длинный автор клипается.
- `editor/src/git/blame.rs:373` — при достижении лимита lookback возвращается `DisplayStart` →
  первая видимая строка продолжающегося run'а помечается `DocumentHead` (метка появляется посреди
  run'а) при >1024 spacer-строк над viewport.
- `workspace/src/dock.rs:1255` — `has_visible_buttons` считает панель с иконкой без
  `icon_tooltip`, а рендер её отбрасывает → разделитель рядом с пустой группой.
- `workspace/src/status_bar.rs:236` — utility-кнопки на внешнем краю `flex_shrink_0` правой группы
  первыми уходят за край узкого окна, вопреки обоснованию «unclippable» в zed.rs.
- `solutions/src/remote_url.rs:120, 95` — `browse_url_suggestion` считает пустой хвостовой сегмент
  (`/tree/`) «чем-то после tree»; свой парсер scp-like расходится с `git::remote::RemoteUrl`
  (`host:path` без `@`), userinfo/port/query сворачиваются в host.
- `solutions/src/folder_name.rs:167` — `APIs` → `API-s`, `iOS` → `i-OS`; правило единое и меняет
  папку для существующих Solution при переименовании в то же имя.
- `solutions_ui/src/project_tab.rs:485` — «Remove Project from Catalog» сначала чистит failed-add,
  потом `remove_catalog_project(..).log_err()`: при устаревшем `catalog_removable` строка исчезает,
  каталог остаётся, фидбека нет.

---

## 2. Производительность

### 2.1 Стриминг: O(транскрипта) на каждый чанк
`crates/solution_agent/src/store/acp_event.rs:857`. На каждый `EntryUpdated`: `rebuild_streams()`
= `demux(&self.entries)` по ВСЕМ записям с `Arc::make_mut` deep-copy голов коалесцированных
групп; затем `persist_main_stream` (store.rs:4352): фильтр `mod_seq > watermark` всегда захватывает
коалесцированную голову (её mod_seq бампится при merge) → `to_payload()` сериализует ВСЁ растущее
сообщение в JSON на foreground и ставит upsert в SQLite на каждый токен. Комментарий в коде
прямо говорит, что 500ms/2s-троттл касается только MCP-эмита, не persist'а. Итог: O(n + len)
на чанк, O(L²) байт записи на сообщение; на длинной сессии — заметные фризы.
**Фикс:** инкрементальный demux (аппендить в существующий стрим, а не пересобирать), троттлить
persist коалесцированной головы (писать по таймеру/на границе сообщения), не копировать Arc-головы.

### 2.2 Render транскрипта: полный обход и N notify на кадр
`crates/solution_agent/src/session_view.rs:1302` + `collect_entry_texts:1140`. Каждый Render:
`entry_text_spans` аллоцирует String на каждый span КАЖДОЙ записи выбранного стрима, затем
`SharedString::from(text.clone())` (вторая копия), `ensure_markdown` сравнивает полное
содержимое, и `set_search_highlights` делает безусловный `cx.notify()` на каждом Markdown-entity
(markdown.rs:811). На 1500-записном/5 MB транскрипте это ~10 MB копий и 1500 notify на кадр при
стриминге, хотя рисуется ~10 строк. Виртуализирован только paint. Плюс `lifecycle.rs:61` +
`session_view.rs:476`: при открытом find-баре `recompute_matches` (полное копирование и скан
текста) запускается дважды на дельту (observe + EntryUpdated).
**Фикс:** кэшировать тексты по `(entry_idx, mod_seq)`; вызывать `set_search_highlights` только при
изменении диапазонов (сравнение до update); recompute find по debounce и один раз.

### 2.3 `event_sources.rs:273` — re-demux префикса на каждый `SessionMessageAppended`
Только чтобы вычислить stream-local индекс. O(n) с deep-copy в том же тике, что и
`rebuild_streams`. Индекс можно брать из уже собранного `streams` по map flat→stream.

### 2.4 Гидрация на foreground
`store/hydration.rs:1588` (и `resume_session:1030`): `entries_from_rows` (serde_json на строку) и
двойной `demux` для КАЖДОЙ сессии Solution внутри одного `this.update` — окно замирает на всё
декодирование. Декодировать/демуксить на background executor до `update`.

### 2.5 MCP-чтения: квадратичный догон и полный рендер ради страницы
`mcp/read.rs:1176` `get_session_changes` суммаризует (полный markdown + base64 картинок) все записи
с `mod_seq > since_seq`, потом режет до 10 → O(behind²/10) при догоне с нуля. `:697` `get_session`
с `count=N` суммаризует весь стрим, потом дропает всё кроме N. `:1602` `read_session_history`
рендерит весь транскрипт (live — внутри `cx.update`), потом `skip/take`. Все — на foreground.
**Фикс:** отбирать индексы до суммаризации; для `count` — идти с хвоста; для history — рендерить
только slice.

### 2.6 Каждый Fetch перезагружает все графы
`crates/project/src/git_store.rs:7450` → `rescan_branches:4993`: безусловно
`initial_graph_data.clear()` + `HeadChanged` даже если список веток не изменился (комментарий всё
ещё говорит «rescan only ever runs after an explicit push»). `GitGraph::invalidate_state` (1294)
чистит `graph_data` и выделение → «Loading» + повторный `git log` (~150 ms на 79k коммитов) на
пустой fetch. Побочно ломает pending-hold (`git_graph_panel.rs:202`): pending-граф без рендера
инвалидируется без рефетча (`git_graph.rs:2230`), `LoadSettled` не приходит, hold доживает до
400 ms и показывает «Loading» — ровно тот blank, ради которого hold введён.
**Фикс:** чистить кэш лога и эмитить `HeadChanged` только при `branch_list_changed` (refs
сдвинулись); в `invalidate_state` для не отрендеренного графа запускать `fetch_initial_graph_data`.

### 2.6a `set_caret_positions` — O(числа буферов) на каждое движение курсора
`crates/multi_buffer/src/multi_buffer.rs:1788`, вызывается из `editor/src/selection.rs:1506` на
каждое изменение выделения. Проход «очистки» по ВСЕМ `self.buffers` с `buffer.update(..)` на каждом
(lease/unlease entity) плюс `self.snapshot(cx)`. В project diff / результатах поиска на 2000+
excerpt'ов стрелка или клик = 2000 entity-update. Док метода сам говорит «must stay cheap».
**Фикс:** помнить множество буферов с кареткой с прошлого вызова и чистить только разницу.

### 2.7 Git-панель / UI: работа на каждый кадр
- `commit_tab.rs:2194` — `ref_row_fit` делает `shape_line` для всех ref-имён + label toggle на
  каждый Render панели (панель ре-рендерится на статус-поллах, hover, скролл): 10–30 шейпингов
  на кадр для релизного коммита. Мемоизировать на `CommitTabState` по (names, row_width, rem).
- `commit_tab.rs:1474` — на каждый settled-выбор спавнится `git tag --points-at`, хотя `%D` уже
  содержит теги (`commit_refs::tag_names`); стрелки по логу = процесс на каждую остановку.
- `solutions_ui/src/project_tab_strip.rs:381` — title bar шейпит все лейблы участников каждый
  кадр окна. Кэш ширин по (name, rem_size).
- `editor/src/git/blame.rs:360` — `run_predecessor_above` каждый layout gutter'а пересканирует
  строки над viewport с удвоением 1..1024, каждая итерация заново собирает `row_infos`,
  `blame_for_rows`, `alignment_rows` — до ~2047 строк и 6 Vec на кадр на панель. Мемоизировать по
  (start_row, snapshot version, blame generation) или сканировать инкрементально.
- `git_ui/src/blame_ui.rs:169` — `sha.to_string()` + 3–4 `editor.read(cx).split_side()` на каждую
  видимую строку blame на кадр. Мелочь, но чистый churn.
- `commit_view.rs:256` — `open(file_filter)` грузит весь коммит (все blob'ы через
  `cat-file --batch`) и `retain`-ит один файл. Латентно (прод-вызовов с `Some` нет).

---

## 3. Архитектура / дублирование (кратко, влияет на будущие баги)

- **Парсинг `%D` в пяти местах** (`commit_refs.rs`, `commit_view/refs_bar.rs:21/63`,
  `branch_submenu.rs:122`, `repository.rs:110/4196`) с расходящимися правилами: remote-рефы в
  CommitView рисуются с namespace, в Commit-табе — без; refs_bar — третий билдер чипов с другими
  глифами. Нужен типизированный `RefDecoration { Head, Local, Remote{..}, Tag }` в точке парсинга
  в `crates/git`.
- `console_panel/panel.rs:981` `band_shows_this_panel` дублирует `SolutionBand::band_state` с
  ДРУГИМ разрешением Solution (`active_solution_id_for_workspace` vs `SolutionBand::solution_id`):
  расхождение = edge авто-старта никогда не срабатывает. Всегда observe band.
- `editor/src/split.rs:738` — `sync_blame_sources` держится на конвенции «каждый мутатор excerpt'ов
  обязан вызвать» (3 вызова + 1 именованное исключение); следующий метод пропустит. Подписка на
  `MultiBufferEvent::ExcerptsAdded|Removed|Expanded` убирает класс регрессий d6b8b3c.
- `solo_diff_view.rs:403` — `DiffSource` плюс теневые `Option` дискриминанты `commit_file`/`remote`,
  9 матчей на `DiffSource::Commit`; `Commit` с `commit_file: None` компилируется и даёт
  полусконфигурированную вью.
- `git_graph_panel.rs:41` — `PendingGraph` + магические 400 ms вместо `Loading{previous}` в модели.
- `store.rs:1618` — `visible_session_count` вычитает `live_supervisor_session_ids`, а
  `first_tab_order_session:1792` и `session_tab_strip::candidates_for:425` — нет: бейдж и число
  вкладок могут разойтись.
- Дубли: три копии `shape_line`-пробы ширины (commit_refs:150, commit_tab:2155,
  project_tab_strip:132); `ref_chips_that_fit` ≡ `fit_count` (уже разошлись по `gap`); три
  фильтр-поповера графа копируют 9 методов + 7 `on_action`; `render_changed_directory_row` не
  использует `render_row_chevron` (иконка папки на другом x).
- `commit_tab.rs:1754` — цикл по `COMMIT_TAB_SECTIONS` — пять последовательных операторов в
  обёртке из enum/const/теста на литерал; `branches`/`tags` — два `LoadState` в lockstep.
- `editor/src/split.rs:424` — `diff_hunk_controls_disabled` читается только из тестов.

## 4. Конвенции
- FORK.md «touched upstream files»: нет строк для `crates/copilot/src/copilot.rs`,
  `crates/remote/src/transport.rs` (+`live_remote_support.rs`), `crates/dap/src/debugger_settings.rs`,
  `crates/util/src/shell.rs`.
- docs/INDEX.md: нет строк для планов `2026-08-31-solution-agent-db-wal`,
  `2026-09-03-blame-run-grouping`, `2026-08-31-disown-the-zed-url-scheme`.
- `store/tests/hydration.rs:3461` — `let _ = send.await;` на `Task<Result<()>>` при утверждении
  «send must go through».
- FORK.md #22 не упоминает, что `OpenTerminal` остался без обработчика (см. 1.1).

## 6. Осталось (сознательно не сделано 2026-09-05)

- **`UnstageAll` / `change_all_files_stage(false)`** по-прежнему делает общий `git reset`, который
  сплющит unmerged-записи так же, как это делал «Mark Unresolved» (см. #154). Не помечен как жест
  снятия разрешения, поэтому в объём не входил.
- **`CommitView::clone_on_split`** всё ещё делит `MultiBuffer` с клоном — исходный предмет TODO C6.
  В `SoloDiffView` это закрыто (клон получает собственный буфер), но `CommitView` подгружает
  excerpt'ы асинхронно по файлам, и клону нужен повтор ещё не пришедших. Debug-assert в
  `display_map` остаётся достижимым через вкладку коммита.
- **`OpenDiff::Commit`** сравнивает по sha без репозитория — та же брешь в идентичности, что была у
  payload'а `CommitTabClosed` (закрыт) и у `DiffSource::matches` (закрыт).
- **`folder_name::derive`**: `APIs` → `API-s`, `iOS` → `i-OS`. Правило единое и запиннено тестами, а
  смена переименовала бы папки существующих Solution. Оставлено намеренно.
- **`CommitView::open(file_filter)`** по-прежнему грузит весь коммит; прод-вызовов с `Some` нет,
  цена задокументирована на месте. Правильный фикс — протащить pathspec в задачу репозитория, это
  правки в `crates/git`/`crates/project`.
- **Ширина строки рефов** не обновляется, пока Commit-таб вообще не отрисован (активен Changes или
  выбрано несколько коммитов): возврат на декорированный коммит после ресайза стоит один
  самоисправляющийся кадр.
- **Windows-ветка правки hand-off** не проверена компилятором — целевой платформы здесь нет.
- **Сброс отложенной записи размеров панелей** на `cx.on_release` сужает окно потери, но не
  закрывает его: порядок разрушения `Workspace` и `Dock` при закрытии окна не гарантирован.
- **`docs/src/terminal.md`** всё ещё описывает терминал как док — неверно для этого форка.
- **Архитектурные пункты §3** (пять парсеров `%D`, дубли `shape_line`/`fit_count`, три копии
  клавиатурной обвязки поповеров, `sync_blame_sources` на конвенции) не трогались: это не баги, и
  каждый — отдельная правка.

## 5. Рекомендуемый порядок работ
1. 1.1 (OpenTerminal), 1.2 (add_member), 1.3 (Mark Unresolved) — потеря данных/функции.
2. 1.4–1.7 (solution_agent lifecycle) — прямые причины «висит Thinking»/«супервизор молчит».
3. 2.1–2.2 (стриминг и render) — главный источник фризов на длинных сессиях; 2.6 (fetch).
4. 1.8–1.9 (diff/commit tab), 1.10–1.11 (CLI).
5. Остальное по списку.
