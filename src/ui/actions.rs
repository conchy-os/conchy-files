//! Команды над файлами (буфер обмена, переименование, корзина, отмена),
//! контекстные меню и выполнение операций в фоне.
//!
//! Одновременно идёт одна операция. Конфликты имён из фонового потока приходят
//! через канал и показываются диалогом; поток ждёт ответа. После операции папка
//! перечитывается, новое выделяется (как в Thunar после вставки).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use conchy_files::model::human_size;
use conchy_files::ops::{self, Answer, Asker, Conflict, JobProgress, Mode, Outcome, Record, Resolution};
use conchy_files::places::Target;
use conchy_files::trash::TrashedItem;
use gpui::{ClipboardItem, Context, Pixels, Point, Task, Timer};

use super::FilesWindow;
use super::dialogs::{ConfirmAction, Dialog, Menu, MenuItem, NamePurpose};
use super::field::TextField;

const UNDO_LIMIT: usize = 100;
const NOTICE_TTL: Duration = Duration::from_secs(4);

/// Команда из меню или с клавиатуры.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Cmd {
    Open,
    OpenTerminal,
    Cut,
    Copy,
    Paste,
    /// Вставить внутрь выделенной папки.
    PasteInto,
    Rename,
    NewFolder,
    NewFile,
    Trash,
    Delete,
    Restore,
    EmptyTrash,
    Undo,
    CopyPath,
    ToggleHidden,
    SelectAll,
    /// Нижняя панель: открыть (свойства выделенного) или переключить.
    ShowPanel,
    TogglePanel,
    /// Устройство из боковой панели (индекс в списке разделов UDisks2).
    OpenDevice(usize),
    Mount(usize),
    Unmount(usize),
    Eject(usize),
    /// Открыть выделенное программой из меню (индекс в `menu_apps`).
    OpenWith(usize),
    OpenWithDialog,
    Redo,
    View(super::grid::ViewMode),
    /// Пользовательское действие из `files.kdl` (индекс в `custom`).
    Custom(usize),
}

/// Что лежит в буфере обмена приложения.
#[derive(Debug, Clone)]
pub(super) struct Clip {
    pub mode: Mode,
    pub paths: Vec<PathBuf>,
}

/// Вопрос о конфликте из фонового потока.
pub(super) struct ConflictRequest {
    pub conflict: Conflict,
    /// «Размер · дата» для того, что уже лежит на месте, и для нового.
    pub dst_info: String,
    pub src_info: String,
    pub reply: mpsc::Sender<Answer>,
}

pub(super) struct Job {
    pub title: String,
    pub progress: Arc<JobProgress>,
    conflicts: mpsc::Receiver<ConflictRequest>,
    _task: Task<()>,
}

#[derive(Debug, Clone)]
pub(super) enum JobKind {
    Normal,
    /// Вставка; после успешного переноса буфер очищается, как в Thunar.
    Paste { cut: bool },
    /// Отмена этой записи; обратная запись уходит в стек «Повторить».
    Undo(Record),
    /// Повтор (отмена отмены); обратная запись возвращается в стек «Отменить».
    Redo(Record),
    /// Операция в корзине: в историю не попадает.
    NoUndo,
}

struct UiAsker(mpsc::Sender<ConflictRequest>);

impl Asker for UiAsker {
    fn ask(&self, c: &Conflict) -> Answer {
        let cancel = Answer { resolution: Resolution::Cancel, apply_to_all: false };
        let (tx, rx) = mpsc::channel();
        let req = ConflictRequest { conflict: c.clone(), dst_info: describe(&c.dst), src_info: describe(&c.src), reply: tx };
        if self.0.send(req).is_err() {
            return cancel;
        }
        // Диалог закрыли, не ответив (или окно закрылось) — считаем отменой.
        rx.recv().unwrap_or(cancel)
    }
}

/// «Размер: 1.5 МБ · Изменён: 12.11.2026 10:41» (для папки — число элементов).
fn describe(path: &Path) -> String {
    let Ok(meta) = fs::symlink_metadata(path) else { return String::new() };
    let what = if meta.is_dir() {
        let n = fs::read_dir(path).map(|d| d.count()).unwrap_or(0);
        format!("Папка, {}", super::items_label(n as u64))
    } else if meta.file_type().is_symlink() {
        let target = fs::read_link(path).map(|t| t.display().to_string()).unwrap_or_default();
        format!("Ссылка → {target}")
    } else {
        format!("Размер: {}", human_size(meta.len()))
    };
    format!("{what}  ·  Изменён: {}", ops::format_mtime(&meta))
}

impl FilesWindow {
    pub(super) fn run_cmd(&mut self, cmd: Cmd, cx: &mut Context<Self>) {
        self.menu = None;
        // В пикере файлы не меняют и не открывают сторонними программами: только выбор.
        if self.pick.is_some()
            && !matches!(
                cmd,
                Cmd::Open | Cmd::Copy | Cmd::NewFolder | Cmd::CopyPath | Cmd::ToggleHidden | Cmd::SelectAll | Cmd::View(_)
            )
        {
            return;
        }
        let trash = self.in_trash();
        match cmd {
            Cmd::Open => self.open_selection(cx),
            Cmd::OpenWith(i) => {
                if let Some(app) = self.menu_apps.get(i).cloned() {
                    let files = self.target_paths();
                    self.launch(&app, &files, cx);
                }
            }
            Cmd::OpenWithDialog => {
                let files = self.target_paths();
                self.open_with_dialog(files, cx);
            }
            Cmd::OpenTerminal => self.open_terminal(cx),
            Cmd::Cut | Cmd::Copy if trash => self.notice("Из корзины файлы можно только восстановить", cx),
            Cmd::Cut | Cmd::Copy => {
                let paths = self.target_paths();
                if paths.is_empty() {
                    return self.notice("Ничего не выделено", cx);
                }
                let mode = if cmd == Cmd::Cut { Mode::Move } else { Mode::Copy };
                cx.write_to_clipboard(ClipboardItem::new_string(join_paths(&paths)));
                let verb = if mode == Mode::Move { "Вырезано" } else { "Скопировано" };
                self.notice(format!("{verb} в буфер: {}", count_label(&paths)), cx);
                self.clipboard = Some(Clip { mode, paths });
            }
            Cmd::Paste | Cmd::PasteInto => {
                let dest = if cmd == Cmd::PasteInto {
                    let rows = self.selected_rows();
                    match rows[..] {
                        [ix] if self.rows[ix].entry.is_dir => Some(self.rows[ix].entry.path.clone()),
                        _ => None,
                    }
                } else {
                    self.current_dir().map(Path::to_path_buf)
                };
                match dest {
                    Some(d) => self.paste(d, cx),
                    None => self.notice("Сюда вставить нельзя", cx),
                }
            }
            Cmd::Rename if trash => {}
            Cmd::Rename => {
                let rows = self.selected_rows();
                let [ix] = rows[..] else {
                    return self.notice("Выделите один элемент, чтобы переименовать", cx);
                };
                let e = &self.rows[ix].entry;
                // Как в Thunar: выделено имя без расширения.
                let base = ops::split_ext(e.name.as_bytes(), e.is_dir).0.len();
                self.dialog = Some(Dialog::Name {
                    purpose: NamePurpose::Rename(e.path.clone()),
                    field: TextField::new(e.name.clone(), 0..base),
                    error: None,
                });
            }
            Cmd::NewFolder | Cmd::NewFile => {
                let Some(parent) = self.current_dir().map(Path::to_path_buf) else { return };
                let is_dir = cmd == Cmd::NewFolder;
                let default = if is_dir { "Новая папка" } else { "Новый файл" };
                let name = ops::next_new_name(&parent, default, is_dir);
                let len = name.len();
                self.dialog = Some(Dialog::Name {
                    purpose: NamePurpose::Create { parent, is_dir },
                    field: TextField::new(name, 0..len),
                    error: None,
                });
            }
            // В корзине Delete значит «удалить насовсем».
            Cmd::Trash if trash => self.run_cmd(Cmd::Delete, cx),
            Cmd::Trash => {
                let paths = self.target_paths();
                if !paths.is_empty() {
                    self.run_job("Перемещение в корзину", JobKind::Normal, cx, move |p, _| ops::trash_all(&paths, p));
                }
            }
            Cmd::Delete => {
                if trash {
                    let items = self.trash_items(true);
                    if !items.is_empty() {
                        let title = delete_title(items.len(), self.selected_name());
                        self.dialog = Some(Dialog::Confirm {
                            title,
                            body: "Элементы будут удалены из корзины, восстановить их будет нельзя.".into(),
                            ok: "Удалить",
                            action: ConfirmAction::Erase(items),
                        });
                    }
                } else {
                    let paths = self.target_paths();
                    if !paths.is_empty() {
                        let title = delete_title(paths.len(), self.selected_name());
                        self.dialog = Some(Dialog::Confirm {
                            title,
                            body: "Элементы не попадут в корзину, восстановить их будет нельзя.".into(),
                            ok: "Удалить",
                            action: ConfirmAction::Delete(paths),
                        });
                    }
                }
            }
            Cmd::Restore => {
                let items = self.trash_items(true);
                if !items.is_empty() {
                    self.run_job("Восстановление", JobKind::NoUndo, cx, move |p, _| ops::restore_all(&items, p));
                }
            }
            Cmd::EmptyTrash => {
                let items = self.trash_items(false);
                if items.is_empty() {
                    return self.notice("Корзина уже пуста", cx);
                }
                self.dialog = Some(Dialog::Confirm {
                    title: "Удалить всё из корзины?".into(),
                    body: format!("{} будут удалены насовсем.", super::items_label(items.len() as u64)),
                    ok: "Очистить корзину",
                    action: ConfirmAction::Erase(items),
                });
            }
            Cmd::Undo => {
                if self.job.is_some() {
                    return self.notice("Дождитесь окончания текущей операции", cx);
                }
                let Some(rec) = self.undo.pop() else {
                    return self.notice("Нечего отменять", cx);
                };
                let title = format!("Отмена: {}", rec.describe());
                let r = rec.clone();
                self.run_job(title, JobKind::Undo(rec), cx, move |p, _| ops::undo(&r, p));
            }
            Cmd::Redo => {
                if self.job.is_some() {
                    return self.notice("Дождитесь окончания текущей операции", cx);
                }
                let Some(rec) = self.redo.pop() else {
                    return self.notice("Нечего повторять", cx);
                };
                let title = format!("Повтор: {}", rec.describe());
                let r = rec.clone();
                self.run_job(title, JobKind::Redo(rec), cx, move |p, _| ops::undo(&r, p));
            }
            Cmd::CopyPath => {
                let paths: Vec<PathBuf> = if trash {
                    self.selected_rows().iter().filter_map(|&i| self.rows[i].entry.trash.as_ref().map(|t| t.original.clone())).collect()
                } else {
                    self.target_paths()
                };
                let paths = if paths.is_empty() { self.current_dir().map(|d| vec![d.to_path_buf()]).unwrap_or_default() } else { paths };
                if !paths.is_empty() {
                    cx.write_to_clipboard(ClipboardItem::new_string(join_paths(&paths)));
                    self.notice("Путь скопирован", cx);
                }
            }
            Cmd::ToggleHidden => self.toggle_hidden(cx),
            Cmd::SelectAll => self.select_all(cx),
            Cmd::ShowPanel => self.show_panel(cx),
            Cmd::View(mode) => self.set_view(mode, cx),
            Cmd::Custom(i) => {
                let Some(a) = self.custom.get(i).cloned() else { return };
                let cwd = self.current_dir().map(Path::to_path_buf).unwrap_or_else(conchy_files::places::home);
                let paths = std::mem::take(&mut self.menu_targets);
                if let Err(e) = a.run(&paths, &cwd) {
                    self.notice(format!("«{}» не запустилось: {e}", a.name), cx);
                }
            }
            Cmd::TogglePanel => self.toggle_panel(cx),
            Cmd::OpenDevice(_) | Cmd::Mount(_) | Cmd::Unmount(_) | Cmd::Eject(_) => self.device_cmd(cmd, cx),
        }
        cx.notify();
    }

    fn paste(&mut self, dest: PathBuf, cx: &mut Context<Self>) {
        // Свой буфер, а если он пуст — пути из системного (например, из терминала).
        let clip = self.clipboard.clone().or_else(|| {
            let text = cx.read_from_clipboard()?.text()?;
            Some(Clip { mode: Mode::Copy, paths: parse_paths(&text)? })
        });
        let Some(clip) = clip else {
            return self.notice("Буфер обмена пуст", cx);
        };
        let cut = clip.mode == Mode::Move;
        let title = if cut { "Перемещение" } else { "Копирование" };
        self.run_job(title, JobKind::Paste { cut }, cx, move |p, a| ops::transfer(clip.mode, &clip.paths, &dest, p, a));
    }

    fn open_terminal(&mut self, cx: &mut Context<Self>) {
        let rows = self.selected_rows();
        let dir = match rows[..] {
            [ix] if self.rows[ix].entry.is_dir => Some(self.rows[ix].entry.path.clone()),
            _ => self.current_dir().map(Path::to_path_buf),
        };
        let Some(dir) = dir else { return };
        match terminal(self.config.terminal.clone()) {
            Some(t) => {
                if let Err(e) = std::process::Command::new(&t).current_dir(&dir).spawn() {
                    self.notice(format!("Не удалось запустить {t}: {e}"), cx);
                }
            }
            None => self.notice("Терминал не найден: укажите terminal.command в files.kdl", cx),
        }
    }

    /// Пути выделенных элементов в порядке показа.
    pub(super) fn target_paths(&self) -> Vec<PathBuf> {
        self.selected_rows().into_iter().map(|i| self.rows[i].entry.path.clone()).collect()
    }

    /// Записи корзины: выделенные или все.
    fn trash_items(&self, only_selected: bool) -> Vec<TrashedItem> {
        let ids: Vec<usize> = if only_selected { self.selected_rows() } else { (0..self.rows.len()).collect() };
        ids.into_iter()
            .filter_map(|i| {
                let e = &self.rows[i].entry;
                let t = e.trash.as_ref()?;
                Some(TrashedItem { original: t.original.clone(), file: e.path.clone(), info: t.info.clone() })
            })
            .collect()
    }

    fn selected_name(&self) -> Option<String> {
        match self.selected_rows()[..] {
            [ix] => Some(self.rows[ix].entry.name.clone()),
            _ => None,
        }
    }

    /// Меню для выделенных элементов (правый щелчок по файлу или папке).
    pub(super) fn open_item_menu(&mut self, pos: Point<Pixels>, cx: &mut Context<Self>) {
        let rows = self.selected_rows();
        let single = rows.len() == 1;
        let single_dir = single && self.rows[rows[0]].entry.is_dir;
        let items = if self.in_trash() {
            vec![
                Some(MenuItem::new("Восстановить", "", Cmd::Restore, true)),
                None,
                Some(MenuItem::new("Удалить насовсем…", "Delete", Cmd::Delete, true).danger()),
                None,
                Some(MenuItem::new("Копировать исходный путь", "", Cmd::CopyPath, true)),
            ]
        } else {
            // Как в Thunar: «Открыть в «Программа»», ещё пара подходящих и «Открыть с помощью…».
            let files: Vec<PathBuf> = rows.iter().filter(|&&i| !self.rows[i].entry.is_dir).map(|&i| self.rows[i].entry.path.clone()).collect();
            let only_files = files.len() == rows.len();
            self.menu_apps = if only_files { self.menu_apps_for(&files) } else { Vec::new() };
            let open_label = match self.menu_apps.first() {
                Some(app) => format!("Открыть в «{}»", app.name),
                None => "Открыть".into(),
            };
            let mut items = vec![Some(MenuItem::new(open_label, "Enter", Cmd::Open, true))];
            for (i, app) in self.menu_apps.iter().enumerate().skip(1).take(2) {
                items.push(Some(MenuItem::new(format!("Открыть в «{}»", app.name), "", Cmd::OpenWith(i), true)));
            }
            if only_files {
                items.push(Some(MenuItem::new("Открыть с помощью другой программы…", "", Cmd::OpenWithDialog, true)));
            }
            items.extend(vec![
                single_dir.then(|| MenuItem::new("Открыть терминал здесь", "", Cmd::OpenTerminal, true)),
                None,
                Some(MenuItem::new("Вырезать", "Ctrl+X", Cmd::Cut, true)),
                Some(MenuItem::new("Копировать", "Ctrl+C", Cmd::Copy, true)),
                single_dir.then(|| MenuItem::new("Вставить в папку", "", Cmd::PasteInto, self.clipboard.is_some())),
                None,
                Some(MenuItem::new("В корзину", "Delete", Cmd::Trash, true)),
                Some(MenuItem::new("Удалить насовсем…", "Shift+Delete", Cmd::Delete, true).danger()),
                None,
                Some(MenuItem::new("Переименовать…", "F2", Cmd::Rename, single)),
                Some(MenuItem::new("Копировать путь", "", Cmd::CopyPath, true)),
                None,
                Some(MenuItem::new(if single_dir { "Анализ места" } else { "Свойства" }, "Alt+Enter", Cmd::ShowPanel, true)),
            ]);
            // Пользовательские действия из files.kdl, подходящие ко всему выделенному.
            let sel: Vec<(PathBuf, bool, String)> =
                rows.iter().map(|&i| (self.rows[i].entry.path.clone(), self.rows[i].entry.is_dir, self.rows[i].entry.mime.clone())).collect();
            let custom: Vec<Option<MenuItem>> = self
                .custom
                .iter()
                .enumerate()
                .filter(|(_, a)| a.applies(&sel))
                .map(|(i, a)| Some(MenuItem::new(a.name.clone(), "", Cmd::Custom(i), true)))
                .collect();
            if !custom.is_empty() {
                items.push(None);
                items.extend(custom);
            }
            self.menu_targets = sel.into_iter().map(|(p, ..)| p).collect();
            items
        };
        self.menu = Some(Menu { pos, items: compact(self.pick_menu(items)) });
        cx.notify();
    }

    /// Меню папки (правый щелчок по пустому месту).
    pub(super) fn open_folder_menu(&mut self, pos: Point<Pixels>, cx: &mut Context<Self>) {
        let items = if self.in_trash() {
            vec![Some(MenuItem::new("Очистить корзину…", "", Cmd::EmptyTrash, !self.rows.is_empty()).danger())]
        } else {
            let undo = self.undo.last().map(|r| format!("Отменить {}", r.describe()));
            vec![
                Some(MenuItem::new("Создать папку…", "Ctrl+Shift+N", Cmd::NewFolder, true)),
                Some(MenuItem::new("Создать пустой файл…", "", Cmd::NewFile, true)),
                None,
                Some(MenuItem::new("Вставить", "Ctrl+V", Cmd::Paste, self.clipboard.is_some())),
                None,
                Some(MenuItem::new("Открыть терминал здесь", "", Cmd::OpenTerminal, true)),
                Some(MenuItem::new("Копировать путь", "", Cmd::CopyPath, true)),
                None,
                Some(MenuItem::new("Выделить всё", "Ctrl+A", Cmd::SelectAll, !self.visible.is_empty())),
                None,
                Some(MenuItem::new("Вид: значки", "Ctrl+1", Cmd::View(super::grid::ViewMode::Icons), self.view_mode != super::grid::ViewMode::Icons)),
                Some(MenuItem::new("Вид: подробно", "Ctrl+2", Cmd::View(super::grid::ViewMode::List), self.view_mode != super::grid::ViewMode::List)),
                Some(MenuItem::new("Вид: компактный", "Ctrl+3", Cmd::View(super::grid::ViewMode::Compact), self.view_mode != super::grid::ViewMode::Compact)),
                Some(MenuItem::new(
                    if self.show_hidden { "Не показывать скрытые" } else { "Показать скрытые" },
                    "Ctrl+H",
                    Cmd::ToggleHidden,
                    true,
                )),
                None,
                Some(MenuItem::new(undo.clone().unwrap_or_else(|| "Отменить".into()), "Ctrl+Z", Cmd::Undo, undo.is_some())),
                Some(MenuItem::new(
                    self.redo.last().map(|r| format!("Повторить {}", r.describe())).unwrap_or_else(|| "Повторить".into()),
                    "Ctrl+Shift+Z",
                    Cmd::Redo,
                    !self.redo.is_empty(),
                )),
                None,
                Some(MenuItem::new(
                    if self.config.panel_visible { "Скрыть панель анализа" } else { "Анализ места" },
                    "Ctrl+I",
                    Cmd::TogglePanel,
                    true,
                )),
            ]
        };
        // Действия с `background true` — для самой папки.
        let mut items = items;
        if let Some(cwd) = self.current_dir().map(Path::to_path_buf) {
            let custom: Vec<Option<MenuItem>> = self
                .custom
                .iter()
                .enumerate()
                .filter(|(_, a)| a.background)
                .map(|(i, a)| Some(MenuItem::new(a.name.clone(), "", Cmd::Custom(i), true)))
                .collect();
            if !custom.is_empty() {
                items.push(None);
                items.extend(custom);
            }
            self.menu_targets = vec![cwd];
        }
        self.menu = Some(Menu { pos, items: compact(self.pick_menu(items)) });
        cx.notify();
    }

    pub(super) fn notice(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        self.notice = Some((text.into(), Instant::now()));
        cx.notify();
        cx.spawn(async move |this, cx| {
            Timer::after(NOTICE_TTL).await;
            let _ = this.update(cx, |_, cx| cx.notify());
        })
        .detach();
    }

    pub(super) fn current_notice(&self) -> Option<&str> {
        self.notice.as_ref().filter(|(_, at)| at.elapsed() < NOTICE_TTL).map(|(s, _)| s.as_str())
    }

    pub(super) fn cancel_job(&mut self) {
        if let Some(job) = &self.job {
            job.progress.cancel();
        }
    }

    pub(super) fn run_job(
        &mut self,
        title: impl Into<String>,
        kind: JobKind,
        cx: &mut Context<Self>,
        work: impl FnOnce(&JobProgress, &dyn Asker) -> Outcome + Send + 'static,
    ) {
        if self.job.is_some() {
            match kind {
                JobKind::Undo(rec) => self.undo.push(rec),
                JobKind::Redo(rec) => self.redo.push(rec),
                _ => {}
            }
            return self.notice("Дождитесь окончания текущей операции", cx);
        }
        let progress = Arc::new(JobProgress::default());
        let (tx, rx) = mpsc::channel();
        let p = progress.clone();
        let bg = cx.background_executor().spawn(async move {
            let asker = UiAsker(tx);
            work(&p, &asker)
        });
        let task = cx.spawn(async move |this, cx| {
            let out = bg.await;
            let _ = this.update(cx, |this, cx| {
                this.finish_job(kind, out, cx);
                cx.notify();
            });
        });
        self.job = Some(Job { title: title.into(), progress, conflicts: rx, _task: task });

        // Пока операция идёт: перерисовываем прогресс и забираем вопросы о конфликтах.
        cx.spawn(async move |this, cx| {
            loop {
                Timer::after(Duration::from_millis(100)).await;
                let running = this
                    .update(cx, |this, cx| {
                        let Some(job) = &this.job else { return false };
                        if this.dialog.is_none() {
                            if let Ok(req) = job.conflicts.try_recv() {
                                this.dialog = Some(Dialog::Conflict { req, apply_all: false });
                            }
                        }
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !running {
                    break;
                }
            }
        })
        .detach();
        cx.notify();
    }

    fn finish_job(&mut self, kind: JobKind, out: Outcome, cx: &mut Context<Self>) {
        let title = self.job.take().map(|j| j.title).unwrap_or_default();
        if matches!(self.dialog, Some(Dialog::Conflict { .. })) {
            self.dialog = None;
        }
        // У отмены и повтора в `record` — обратная запись, она же говорит, что появилось.
        let select = out.record.as_ref().map(new_paths).unwrap_or_default();
        // Деревья анализа места обновляем точечно, без полного пересканирования.
        self.patch_analyses(out.touched.clone(), cx);
        let here: Vec<PathBuf> = match &self.location {
            Target::Dir(d) => select.into_iter().filter(|p| p.parent() == Some(d.as_path())).collect(),
            Target::Trash => Vec::new(),
        };
        self.load(here, cx);

        if let JobKind::Paste { cut: true } = kind {
            if out.record.is_some() {
                self.clipboard = None;
            }
        }
        if let Some(r) = out.record {
            match kind {
                JobKind::Normal | JobKind::Paste { .. } => {
                    self.undo.push(r);
                    // Новое действие обрывает цепочку повторов, как в любом редакторе.
                    self.redo.clear();
                }
                JobKind::Undo(_) => self.redo.push(r),
                JobKind::Redo(_) => self.undo.push(r),
                JobKind::NoUndo => {}
            }
            if self.undo.len() > UNDO_LIMIT {
                self.undo.remove(0);
            }
        }
        if !out.errors.is_empty() {
            self.dialog = Some(Dialog::Errors { title: format!("{title}: не всё получилось"), errors: out.errors });
        } else if out.cancelled {
            self.notice(format!("{title}: прервано"), cx);
        } else {
            self.notice(format!("{title}: готово"), cx);
        }
    }
}

/// Убирает пустые пункты и повторяющиеся или крайние разделители.
fn compact(items: Vec<Option<MenuItem>>) -> Vec<Option<MenuItem>> {
    let mut out: Vec<Option<MenuItem>> = Vec::new();
    for it in items {
        match it {
            Some(i) => out.push(Some(i)),
            None if out.last().is_some_and(Option::is_some) => out.push(None),
            None => {}
        }
    }
    while out.last().is_some_and(Option::is_none) {
        out.pop();
    }
    out
}

fn delete_title(n: usize, single: Option<String>) -> String {
    match single {
        Some(name) if n == 1 => format!("Удалить насовсем «{name}»?"),
        _ => format!("Удалить насовсем {}?", super::items_label(n as u64)),
    }
}

/// Программа-терминал: из `files.kdl`, иначе `$TERMINAL`, иначе первый найденный из известных.
pub(super) fn terminal(configured: Option<String>) -> Option<String> {
    if let Some(t) = configured.or_else(|| std::env::var("TERMINAL").ok()).filter(|t| !t.trim().is_empty()) {
        return Some(t);
    }
    let path = std::env::var_os("PATH")?;
    ["foot", "alacritty", "kitty", "wezterm", "ghostty", "konsole", "gnome-terminal", "xfce4-terminal", "xterm"]
        .into_iter()
        .find(|t| std::env::split_paths(&path).any(|d| d.join(t).is_file()))
        .map(String::from)
}

/// Что выделить после операции: то, что появилось.
fn new_paths(r: &Record) -> Vec<PathBuf> {
    match r {
        Record::Copied(v) => v.clone(),
        Record::Moved(v) => v.iter().map(|(_, to)| to.clone()).collect(),
        Record::Renamed { to, .. } => vec![to.clone()],
        Record::Created(p) => vec![p.clone()],
        Record::Restored(v) => v.clone(),
        Record::Trashed(_) | Record::Removed { .. } => Vec::new(),
    }
}

fn join_paths(paths: &[PathBuf]) -> String {
    paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("\n")
}

fn count_label(paths: &[PathBuf]) -> String {
    match paths {
        [one] => format!("«{}»", ops::display_name(one)),
        _ => super::items_label(paths.len() as u64),
    }
}

/// Пути из текста буфера: по одному на строку, абсолютные или `file://`, все существуют.
fn parse_paths(text: &str) -> Option<Vec<PathBuf>> {
    use std::os::unix::ffi::OsStringExt;
    let mut out = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let p = match line.strip_prefix("file://") {
            Some(rest) => PathBuf::from(std::ffi::OsString::from_vec(conchy_files::percent_decode(rest))),
            None => PathBuf::from(line),
        };
        if !p.is_absolute() || fs::symlink_metadata(&p).is_err() {
            return None;
        }
        out.push(p);
    }
    (!out.is_empty()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_clipboard_paths() {
        assert_eq!(conchy_files::percent_decode("/tmp/a%20b"), b"/tmp/a b");
        assert_eq!(conchy_files::percent_decode("/x%2"), b"/x%2");
        assert_eq!(parse_paths("/\nfile:///tmp\n"), Some(vec![PathBuf::from("/"), PathBuf::from("/tmp")]));
        assert_eq!(parse_paths("просто текст"), None);
        assert_eq!(parse_paths(""), None);
    }

    #[test]
    fn menu_separators_are_tidy() {
        let it = || Some(MenuItem::new("x", "", Cmd::Open, true));
        let v = compact(vec![None, it(), None, None, None, it(), None]);
        assert_eq!(v.iter().map(Option::is_some).collect::<Vec<_>>(), [true, false, true]);
    }
}
