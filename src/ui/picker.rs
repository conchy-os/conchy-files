//! Режим «пикер» окна conchy-files: диалог выбора файла для портала FileChooser.
//!
//! Это то же окно со списком, боковой панелью и сеткой, что и обычный файловый менеджер;
//! добавляется нижняя панель (имя файла, фильтр, выборы, кнопки), фильтрация списка на уровне
//! модели и ответ демону портала. Запросы приходят из [`conchy_files::portal_host`].

use std::path::PathBuf;
use std::time::Instant;

use conchy_files::dir::Entry;
use conchy_files::pick::{self as logic, Choice, ChoiceSelection, Filter, FilePickRequest, FilePickResult};
use conchy_files::{mime, places};
use conchy_portal_proto::response;
use futures::channel::oneshot;
use gpui::{AnyElement, ClickEvent, Context, KeyDownEvent, MouseButton, MouseDownEvent, div, prelude::*, px, rgb};

use super::actions::Cmd;
use super::dialogs::{ConfirmAction, Dialog, MenuItem};
use super::field::TextField;
use super::{ACCENT, BG, BORDER, DIM, FilesWindow, PANEL, SEL_BG, TEXT};

/// Открытый выпадающий список над нижней панелью.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Popup {
    Filter,
    Choice(usize),
}

pub(super) struct PickState {
    pub id: String,
    pub req: FilePickRequest,
    reply: Option<oneshot::Sender<(u32, FilePickResult)>>,
    label: String,
    /// Поле имени (только Save).
    pub name: Option<TextField>,
    /// Клавиатура идёт в поле имени, а не в список.
    pub name_focus: bool,
    /// Индекс активного фильтра в `req.filters`.
    pub filter: Option<usize>,
    choices: Vec<(Choice, String)>,
    popup: Option<Popup>,
    state_file: PathBuf,
    started: Instant,
    first_frame_logged: bool,
    /// Окно пикера: закрывается из любого места, где нет доступа к `Window`.
    window: Option<gpui::AnyWindowHandle>,
}

impl PickState {
    pub fn new(id: String, req: FilePickRequest, reply: oneshot::Sender<(u32, FilePickResult)>) -> Self {
        let name = (req.kind == logic::KIND_SAVE).then(|| {
            let text = if !req.current_name.is_empty() {
                req.current_name.clone()
            } else {
                // Для «Сохранить как» по существующему файлу — его имя.
                logic::path_from_bytes(&req.current_file)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            };
            // Как в Thunar при переименовании: выделено имя без расширения.
            let stem = text.rfind('.').filter(|&i| i > 0).unwrap_or(text.len());
            TextField::new(text, 0..stem)
        });
        let filter = req
            .current_filter
            .first()
            .and_then(|cf| req.filters.iter().position(|f| f == cf))
            .or_else(|| (!req.filters.is_empty()).then_some(0));
        let choices = req.choices.iter().map(|c| (c.clone(), logic::choice_initial(c))).collect();
        Self {
            id,
            label: logic::accept_label(&req),
            name_focus: name.is_some(),
            name,
            filter,
            choices,
            popup: None,
            state_file: logic::state_file(),
            started: Instant::now(),
            first_frame_logged: false,
            window: None,
            req,
            reply: None,
        }
        .with_reply(reply)
    }

    fn with_reply(mut self, reply: oneshot::Sender<(u32, FilePickResult)>) -> Self {
        self.reply = Some(reply);
        self
    }

    fn active_filter(&self) -> Option<&Filter> {
        self.filter.and_then(|i| self.req.filters.get(i))
    }

    fn dirs_only(&self) -> bool {
        match self.req.kind {
            logic::KIND_SAVE_FILES => true,
            logic::KIND_OPEN => self.req.directory,
            _ => false,
        }
    }
}

impl FilesWindow {
    /// Переводит окно в режим пикера. Стартовую папку надо выбрать до создания окна
    /// (см. [`logic::start_folder`]).
    pub(super) fn enter_pick_mode(&mut self, mut state: PickState, window: &gpui::Window, cx: &mut Context<Self>) {
        state.window = Some(window.window_handle());
        self.pick = Some(state);
        self.rebuild_visible();
        // Окно закрыли (крестик, wm, падение) без ответа — демон получает «отмена».
        cx.on_release(|this, _| this.pick_abandon()).detach();
        cx.notify();
    }

    /// Фильтр списка на уровне модели: папки видны всегда (если пикер не «только папки»,
    /// то файлы скрываются), файлы — по активному фильтру.
    pub(super) fn pick_hides(&self, e: &Entry) -> bool {
        let Some(p) = &self.pick else { return false };
        if e.is_dir {
            return false;
        }
        if p.dirs_only() {
            return true;
        }
        p.active_filter().is_some_and(|f| !logic::filter_matches(f, &e.name, &e.mime, mime::db()))
    }

    pub(super) fn pick_title(&self) -> Option<String> {
        let p = self.pick.as_ref()?;
        Some(if p.req.title.is_empty() { p.label.clone() } else { p.req.title.clone() })
    }

    /// Папка или файл по двойному щелчку / Enter. `true` — событие обработано.
    pub(super) fn pick_open_row(&mut self, ix: usize, cx: &mut Context<Self>) -> bool {
        let Some(p) = &self.pick else { return false };
        let e = &self.rows[ix].entry;
        if e.is_dir {
            return false; // войти в папку — обычное поведение
        }
        if p.req.kind == logic::KIND_SAVE {
            let name = e.name.clone();
            self.pick_set_name(&name);
        } else {
            self.selected.clear();
            self.selected.insert(e.path.clone());
        }
        self.pick_accept(cx);
        true
    }

    /// Enter при выделении: одна папка — войти, иначе принять.
    pub(super) fn pick_enter(&mut self, cx: &mut Context<Self>) {
        let rows = self.selected_rows();
        if rows.len() == 1 && self.rows[rows[0]].entry.is_dir {
            return self.open_row(rows[0], cx);
        }
        if rows.len() == 1 && self.pick.as_ref().is_some_and(|p| p.req.kind == logic::KIND_SAVE) {
            let name = self.rows[rows[0]].entry.name.clone();
            self.pick_set_name(&name);
        }
        self.pick_accept(cx);
    }

    /// Щелчок по файлу в Save подставляет его имя в поле.
    pub(super) fn pick_clicked(&mut self, ix: usize) {
        if self.pick.as_ref().is_some_and(|p| p.req.kind == logic::KIND_SAVE) && !self.rows[ix].entry.is_dir {
            let name = self.rows[ix].entry.name.clone();
            self.pick_set_name(&name);
        }
    }

    fn pick_set_name(&mut self, name: &str) {
        if let Some(p) = &mut self.pick {
            let stem = name.rfind('.').filter(|&i| i > 0).unwrap_or(name.len());
            p.name = Some(TextField::new(name, 0..stem));
        }
    }

    /// Мультивыбор доступен, только если портал его разрешил.
    pub(super) fn pick_single_only(&self) -> bool {
        self.pick.as_ref().is_some_and(|p| !(p.req.kind == logic::KIND_OPEN && p.req.multiple))
    }

    /// Клавиши при фокусе в поле имени.
    pub(super) fn on_pick_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        let Some(p) = &mut self.pick else { return false };
        if !p.name_focus {
            return false;
        }
        let ks = &ev.keystroke;
        cx.stop_propagation();
        match ks.key.as_str() {
            "escape" => {
                if p.popup.take().is_none() {
                    self.pick_cancel(cx);
                }
            }
            "enter" => self.pick_accept(cx),
            "tab" => p.name_focus = false,
            _ => {
                if let Some(f) = &mut p.name {
                    let m = &ks.modifiers;
                    if (ks.key == "v" && m.control) || (ks.key == "insert" && m.shift) {
                        if let Some(t) = cx.read_from_clipboard().and_then(|c| c.text()) {
                            f.insert(t.lines().next().unwrap_or(""));
                        }
                    } else {
                        f.key(ks);
                    }
                }
            }
        }
        cx.notify();
        true
    }

    /// Esc: закрыть выпадающий список, иначе отменить выбор.
    pub(super) fn pick_escape(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(p) = &mut self.pick else { return false };
        if p.popup.take().is_none() {
            self.pick_cancel(cx);
        }
        cx.notify();
        true
    }

    pub(super) fn pick_cancel(&mut self, cx: &mut Context<Self>) {
        self.pick_finish(response::CANCELLED, Vec::new(), cx);
    }

    /// Принять выбор («Открыть»/«Сохранить»/«Выбрать»).
    pub(super) fn pick_accept(&mut self, cx: &mut Context<Self>) {
        let Some(p) = &self.pick else { return };
        match p.req.kind {
            logic::KIND_SAVE => self.accept_save(cx),
            logic::KIND_SAVE_FILES => {
                let Some(folder) = self.chosen_folder() else {
                    return self.notice("Выберите папку", cx);
                };
                // Имена файлов дописываются к папке; занятые — получают свободное имя.
                let paths: Vec<PathBuf> = p
                    .req
                    .files
                    .iter()
                    .map(|f| {
                        let name = logic::path_from_bytes(f).file_name().map(|n| n.to_string_lossy().into_owned());
                        let name = name.unwrap_or_default();
                        folder.join(logic::unique_name(&folder, &name))
                    })
                    .collect();
                self.pick_finish(response::OK, paths, cx);
            }
            _ if p.req.directory => {
                let dirs: Vec<PathBuf> = self
                    .selected_rows()
                    .into_iter()
                    .filter(|&i| self.rows[i].entry.is_dir)
                    .map(|i| self.rows[i].entry.path.clone())
                    .collect();
                let dirs = if dirs.is_empty() { self.current_dir().map(|d| vec![d.to_path_buf()]).unwrap_or_default() } else { dirs };
                if dirs.is_empty() {
                    return self.notice("Выберите папку", cx);
                }
                let single = !p.req.multiple;
                self.pick_finish(response::OK, if single { dirs.into_iter().take(1).collect() } else { dirs }, cx);
            }
            _ => {
                let files: Vec<PathBuf> = self
                    .selected_rows()
                    .into_iter()
                    .filter(|&i| !self.rows[i].entry.is_dir && !self.rows[i].entry.broken)
                    .map(|i| self.rows[i].entry.path.clone())
                    .collect();
                if files.is_empty() {
                    return self.notice("Выберите файл", cx);
                }
                self.pick_finish(response::OK, files, cx);
            }
        }
    }

    /// Выделенная папка или текущая.
    fn chosen_folder(&self) -> Option<PathBuf> {
        let sel = self.selected_rows();
        match sel.as_slice() {
            [i] if self.rows[*i].entry.is_dir => Some(self.rows[*i].entry.path.clone()),
            _ => self.current_dir().map(PathBuf::from),
        }
    }

    fn accept_save(&mut self, cx: &mut Context<Self>) {
        let name = self.pick.as_ref().and_then(|p| p.name.as_ref()).map(|f| f.text.trim().to_string()).unwrap_or_default();
        if name.is_empty() {
            return self.notice("Введите имя файла", cx);
        }
        let target = if name.starts_with('/') {
            PathBuf::from(&name)
        } else if let Some(rest) = name.strip_prefix('~') {
            places::home().join(rest.trim_start_matches('/'))
        } else {
            let Some(folder) = self.current_dir() else {
                return self.notice("Сюда сохранять нельзя", cx);
            };
            match logic::save_target(folder, &name) {
                Ok(p) => p,
                Err(e) => return self.notice(e, cx),
            }
        };
        match std::fs::symlink_metadata(&target) {
            // Ввели имя папки — входим в неё, как в диалогах GTK.
            Ok(m) if m.is_dir() => {
                if let Some(p) = &mut self.pick {
                    p.name = Some(TextField::new("", 0..0));
                }
                self.navigate(places::Target::Dir(target), cx);
            }
            Ok(_) => {
                let shown = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                self.dialog = Some(Dialog::Confirm {
                    title: format!("Заменить «{shown}»?"),
                    body: "Файл с таким именем уже существует. Его содержимое будет заменено.".into(),
                    ok: "Заменить",
                    action: ConfirmAction::Overwrite(target),
                });
                cx.notify();
            }
            Err(_) => {
                if target.parent().is_some_and(|d| d.is_dir()) {
                    self.pick_finish(response::OK, vec![target], cx);
                } else {
                    self.notice("Такой папки нет", cx);
                }
            }
        }
    }

    /// Ответ демону и закрытие окна. Повторный вызов ничего не делает.
    pub(super) fn pick_finish(&mut self, code: u32, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let folder = self.current_dir().map(PathBuf::from);
        let Some(p) = &mut self.pick else { return };
        let Some(reply) = p.reply.take() else { return };
        let result = if code == response::OK {
            if let Some(folder) = &folder {
                let _ = logic::remember_folder(&p.state_file, &p.req.app_id, folder);
            }
            FilePickResult {
                paths: paths.iter().map(|f| logic::path_to_bytes(f)).collect(),
                choices: p.choices.iter().map(|(c, v)| ChoiceSelection { id: c.id.clone(), value: v.clone() }).collect(),
                current_filter: p.active_filter().cloned().into_iter().collect(),
                writable: true,
            }
        } else {
            FilePickResult::default()
        };
        let _ = reply.send((code, result));
        // `remove_window` нужен `Window`, которого в `Context` нет: закрываем через handle.
        if let Some(handle) = p.window {
            cx.defer(move |cx| {
                let _ = handle.update(cx, |_, window, _| window.remove_window());
            });
        }
    }

    /// Окно ушло без ответа: отвечаем «отмена», чтобы демон не ждал вечно.
    fn pick_abandon(&mut self) {
        if let Some(reply) = self.pick.as_mut().and_then(|p| p.reply.take()) {
            let _ = reply.send((response::CANCELLED, FilePickResult::default()));
        }
    }

    /// Контекстное меню пикера: только выбор и навигация, без удаления и открытия программами.
    pub(super) fn pick_menu(&self, items: Vec<Option<MenuItem>>) -> Vec<Option<MenuItem>> {
        let Some(p) = &self.pick else { return items };
        items
            .into_iter()
            .map(|i| {
                i.filter(|m| {
                    matches!(
                        m.cmd,
                        Cmd::Open | Cmd::Copy | Cmd::NewFolder | Cmd::CopyPath | Cmd::ToggleHidden | Cmd::SelectAll | Cmd::View(_)
                    )
                })
                .map(|mut m| {
                    if matches!(m.cmd, Cmd::Open) {
                        m.label = p.label.clone();
                    }
                    m
                })
            })
            .collect()
    }

    /// Id запроса, которому принадлежит окно (для `Cancel` от демона).
    pub(super) fn pick_id(&self) -> Option<&str> {
        self.pick.as_ref().map(|p| p.id.as_str())
    }

    /// Первый кадр пикера: меряем время от вызова портала (бюджет — 80 мс при тёплом хосте).
    pub(super) fn pick_frame_drawn(&mut self) {
        let Some(p) = self.pick.as_mut().filter(|p| !p.first_frame_logged) else { return };
        p.first_frame_logged = true;
        if std::env::var_os("CONCHY_PORTAL_TRACE").is_some() {
            eprintln!("[conchy-files] picker first frame: {:.1} ms", p.started.elapsed().as_secs_f64() * 1000.0);
        }
    }

    pub(super) fn set_pick_filter(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(p) = &mut self.pick {
            p.filter = Some(ix);
            p.popup = None;
        }
        self.rebuild_visible();
        // Невидимое не может быть выбрано.
        let present: std::collections::HashSet<&PathBuf> = self.visible.iter().map(|&i| &self.rows[i].entry.path).collect();
        self.selected.retain(|p| present.contains(p));
        cx.notify();
    }

    /// Нижняя панель пикера.
    pub(super) fn render_pick_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let p = self.pick.as_ref()?;
        let btn = |id: &'static str, label: String, primary: bool| {
            div()
                .id(id)
                .px_4()
                .py_1()
                .rounded_md()
                .border_1()
                .flex_shrink_0()
                .cursor_pointer()
                .when(primary, |d| d.bg(rgb(ACCENT)).border_color(rgb(ACCENT)).text_color(rgb(BG)))
                .when(!primary, |d| d.border_color(rgb(BORDER)).hover(|s| s.bg(rgb(BORDER))))
                .child(label)
        };
        let dropdown = |id: &'static str, text: String| {
            div()
                .id(id)
                .px_3()
                .py_1()
                .rounded_md()
                .border_1()
                .border_color(rgb(BORDER))
                .cursor_pointer()
                .hover(|s| s.bg(rgb(BORDER)))
                .flex_shrink_0()
                .child(format!("{text}  ▾"))
        };

        let mut controls = div().flex().items_center().gap_3().w_full();
        if !p.req.filters.is_empty() {
            let name = p.active_filter().map(|f| f.name.clone()).unwrap_or_default();
            let mut wrap = div().relative().child(
                dropdown("pick-filter", name).on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                    if let Some(p) = &mut this.pick {
                        p.popup = (p.popup != Some(Popup::Filter)).then_some(Popup::Filter);
                    }
                    cx.notify();
                })),
            );
            if p.popup == Some(Popup::Filter) {
                let items = p.req.filters.iter().enumerate().map(|(i, f)| (f.name.clone(), p.filter == Some(i)));
                wrap = wrap.child(popup_list(items, cx, |this, i, cx| this.set_pick_filter(i, cx)));
            }
            controls = controls.child(wrap);
        }
        for (ci, (c, value)) in p.choices.iter().enumerate() {
            if c.options.is_empty() {
                let on = value == "true";
                controls = controls.child(
                    div()
                        .id(("pick-choice", ci))
                        .flex()
                        .items_center()
                        .gap_1()
                        .flex_shrink_0()
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            if let Some((_, v)) = this.pick.as_mut().and_then(|p| p.choices.get_mut(ci)) {
                                *v = if *v == "true" { "false".into() } else { "true".into() };
                            }
                            cx.notify();
                        }))
                        .child(if on { "☑" } else { "☐" })
                        .child(c.label.clone()),
                );
            } else {
                let current = c.options.iter().find(|o| &o.id == value).map_or(value.clone(), |o| o.label.clone());
                let mut wrap = div().relative().flex().items_center().gap_2().child(div().text_color(rgb(DIM)).child(format!("{}:", c.label))).child(
                    dropdown("pick-combo", current).on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        if let Some(p) = &mut this.pick {
                            p.popup = (p.popup != Some(Popup::Choice(ci))).then_some(Popup::Choice(ci));
                        }
                        cx.notify();
                    })),
                );
                if p.popup == Some(Popup::Choice(ci)) {
                    let items = c.options.iter().map(|o| (o.label.clone(), &o.id == value));
                    wrap = wrap.child(popup_list(items, cx, move |this, i, cx| {
                        if let Some(p) = &mut this.pick {
                            if let Some((c, v)) = p.choices.get_mut(ci)
                                && let Some(o) = c.options.get(i)
                            {
                                *v = o.id.clone();
                            }
                            p.popup = None;
                        }
                        cx.notify();
                    }));
                }
                controls = controls.child(wrap);
            }
        }
        controls = controls
            .child(div().flex_1())
            .child(btn("pick-cancel", "Отмена".into(), false).on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.pick_cancel(cx))))
            .child(btn("pick-accept", p.label.clone(), true).on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.pick_accept(cx))));

        let mut col = div()
            .flex()
            .flex_col()
            .gap_2()
            .px_3()
            .py_2()
            .flex_shrink_0()
            .bg(rgb(PANEL))
            .border_t_1()
            .border_color(rgb(BORDER));
        if let Some(f) = &p.name {
            let focused = p.name_focus;
            col = col.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().w(px(48.)).text_color(rgb(DIM)).child("Имя:"))
                    .child(
                        div()
                            .flex_1()
                            .when(!focused, |d| d.opacity(0.7))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _: &MouseDownEvent, _, cx| {
                                    if let Some(p) = &mut this.pick {
                                        p.name_focus = true;
                                    }
                                    cx.notify();
                                }),
                            )
                            .child(f.render()),
                    ),
            );
        }
        Some(col.child(controls).into_any_element())
    }
}

/// Список вариантов, раскрытый вверх от кнопки.
fn popup_list(
    items: impl Iterator<Item = (String, bool)>,
    cx: &mut Context<FilesWindow>,
    pick: impl Fn(&mut FilesWindow, usize, &mut Context<FilesWindow>) + Clone + 'static,
) -> AnyElement {
    let mut list = div()
        .absolute()
        .bottom(px(32.))
        .left_0()
        .min_w(px(180.))
        .flex()
        .flex_col()
        .p_1()
        .bg(rgb(PANEL))
        .border_1()
        .border_color(rgb(BORDER))
        .rounded_md()
        .shadow_lg()
        .occlude();
    for (i, (label, selected)) in items.enumerate() {
        let pick = pick.clone();
        list = list.child(
            div()
                .id(("pick-opt", i))
                .px_2()
                .py_1()
                .rounded_sm()
                .cursor_pointer()
                .whitespace_nowrap()
                .when(selected, |d| d.bg(rgb(SEL_BG)).text_color(rgb(TEXT)))
                .hover(|s| s.bg(rgb(BORDER)))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| pick(this, i, cx)))
                .child(label),
        );
    }
    list.into_any_element()
}

/// Открывает окно-пикер. `reply` получит ответ, когда пользователь закончит (или окно закроют).
pub fn open_picker(
    cx: &mut gpui::App,
    id: String,
    req: FilePickRequest,
    reply: oneshot::Sender<(u32, FilePickResult)>,
    settings: &std::sync::Arc<conchy_files::settings::Settings>,
) {
    let config = settings.load().unwrap_or_default();
    let last = logic::last_folder(&logic::state_file(), &req.app_id);
    let start = logic::start_folder(&req, last, &places::home());
    let bounds = gpui::Bounds::centered(None, gpui::size(px(900.), px(600.)), cx);
    let settings = settings.clone();
    let state = PickState::new(id, req, reply);
    let handle = cx.open_window(
        gpui::WindowOptions {
            window_bounds: Some(gpui::WindowBounds::Windowed(bounds)),
            app_id: Some("conchy-files-picker".into()),
            ..Default::default()
        },
        move |window, cx| {
            cx.new(|cx| {
                let mut w = FilesWindow::new(start, false, settings, config, window, cx);
                w.enter_pick_mode(state, window, cx);
                w
            })
        },
    );
    if let Ok(handle) = handle {
        let _ = handle.update(cx, |_, window, _| window.activate_window());
    }
}

/// Фронтенд закрыл запрос: закрыть пикер с этим id (ответ демону — «отмена»).
pub fn cancel_pick(cx: &mut gpui::App, id: &str) {
    for w in cx.windows() {
        if let Some(h) = w.downcast::<FilesWindow>() {
            let _ = h.update(cx, |this, _, cx| {
                if this.pick_id() == Some(id) {
                    this.pick_cancel(cx);
                }
            });
        }
    }
}
