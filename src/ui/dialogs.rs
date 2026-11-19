//! Модальные диалоги (имя, подтверждение, конфликт, ошибки) и контекстное меню.
//! Всё рисуется поверх окна.

use std::path::PathBuf;

use conchy_files::ops::{self, Answer, Resolution};
use gpui::{
    AnyElement, ClickEvent, Context, Div, KeyDownEvent, MouseButton, MouseDownEvent, Pixels, Point, Stateful, Window, div,
    hsla, prelude::*, px, rgb,
};

use super::actions::{Cmd, ConflictRequest, JobKind};
use super::field::TextField;
use conchy_files::trash::TrashedItem;

use super::{ACCENT, BG, BORDER, DANGER, DIM, FilesWindow, PANEL, SEL_BG, TEXT};

pub(super) enum Dialog {
    /// Ввод имени: переименование или создание.
    Name { purpose: NamePurpose, field: TextField, error: Option<String> },
    /// Подтверждение необратимого действия.
    Confirm { title: String, body: String, ok: &'static str, action: ConfirmAction },
    Conflict { req: ConflictRequest, apply_all: bool },
    Errors { title: String, errors: Vec<(PathBuf, String)> },
    OpenWith(super::openwith::OpenWith),
    /// Пароль для сетевой папки.
    Password { remote: conchy_files::remote::Remote, field: TextField, error: Option<String> },
}

pub(super) enum NamePurpose {
    Rename(PathBuf),
    Create { parent: PathBuf, is_dir: bool },
}

pub(super) enum ConfirmAction {
    /// Удалить насовсем, минуя корзину.
    Delete(Vec<PathBuf>),
    /// Удалить из корзины (в том числе «Очистить корзину»).
    Erase(Vec<TrashedItem>),
    /// Пикер: заменить существующий файл при сохранении.
    Overwrite(PathBuf),
}

/// Открытое контекстное меню: где щёлкнули и что в нём (`None` — разделитель).
pub(super) struct Menu {
    pub pos: Point<Pixels>,
    pub items: Vec<Option<MenuItem>>,
}

pub(super) struct MenuItem {
    pub label: String,
    pub keys: &'static str,
    pub cmd: Cmd,
    pub enabled: bool,
    pub danger: bool,
}

impl MenuItem {
    pub fn new(label: impl Into<String>, keys: &'static str, cmd: Cmd, enabled: bool) -> Self {
        MenuItem { label: label.into(), keys, cmd, enabled, danger: false }
    }

    pub fn danger(mut self) -> Self {
        self.danger = true;
        self
    }
}

const MENU_W: f32 = 260.0;
const MENU_ITEM_H: f32 = 26.0;

impl FilesWindow {
    pub(super) fn on_dialog_key(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.dialog.is_none() {
            return;
        }
        let ks = &ev.keystroke;
        cx.stop_propagation();
        match ks.key.as_str() {
            "escape" => return self.dialog_cancel(cx),
            "enter" => return self.dialog_confirm(cx),
            _ => {}
        }
        if matches!(self.dialog, Some(Dialog::OpenWith(_))) {
            return self.open_with_key(ev, cx);
        }
        match &mut self.dialog {
            Some(Dialog::Password { field, .. }) => {
                field.key(ks);
            }
            Some(Dialog::Name { field, error, .. }) => {
                let m = &ks.modifiers;
                if (ks.key == "v" && m.control) || (ks.key == "insert" && m.shift) {
                    if let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) {
                        field.insert(text.lines().next().unwrap_or(""));
                        *error = None;
                    }
                } else if field.key(ks) {
                    *error = None;
                }
            }
            Some(Dialog::Conflict { apply_all, .. }) if ks.key == "space" => *apply_all = !*apply_all,
            _ => {}
        }
        cx.notify();
    }

    fn dialog_cancel(&mut self, cx: &mut Context<Self>) {
        if let Some(Dialog::Conflict { req, .. }) = self.dialog.take() {
            let _ = req.reply.send(Answer { resolution: Resolution::Cancel, apply_to_all: false });
        }
        cx.notify();
    }

    fn dialog_confirm(&mut self, cx: &mut Context<Self>) {
        match self.dialog.take() {
            Some(Dialog::Name { purpose, field, .. }) => {
                let name = field.text.clone();
                let parent = match &purpose {
                    NamePurpose::Rename(p) => p.parent().map(PathBuf::from).unwrap_or_default(),
                    NamePurpose::Create { parent, .. } => parent.clone(),
                };
                let same = matches!(&purpose, NamePurpose::Rename(p) if p.file_name().is_some_and(|n| *n == *name));
                let err = ops::validate_name(&name).err().or_else(|| {
                    let target = parent.join(&name);
                    let taken = std::fs::symlink_metadata(&target).is_ok();
                    // Смена только регистра у того же файла — не конфликт.
                    let self_case = matches!(&purpose, NamePurpose::Rename(p)
                        if p.to_string_lossy().to_lowercase() == target.to_string_lossy().to_lowercase());
                    (!same && taken && !self_case).then(|| format!("«{name}» уже существует"))
                });
                if let Some(e) = err {
                    self.dialog = Some(Dialog::Name { purpose, field, error: Some(e) });
                } else if !same {
                    match purpose {
                        NamePurpose::Rename(path) => {
                            self.run_job("Переименование", JobKind::Normal, cx, move |_, _| ops::rename(&path, &name))
                        }
                        NamePurpose::Create { parent, is_dir } => {
                            let title = if is_dir { "Создание папки" } else { "Создание файла" };
                            self.run_job(title, JobKind::Normal, cx, move |_, _| ops::create(&parent, &name, is_dir))
                        }
                    }
                }
            }
            Some(Dialog::Confirm { action: ConfirmAction::Delete(paths), .. }) => {
                self.run_job("Удаление", JobKind::NoUndo, cx, move |p, _| ops::delete_all(&paths, p));
            }
            Some(Dialog::Confirm { action: ConfirmAction::Overwrite(path), .. }) => {
                self.pick_finish(conchy_portal_proto::response::OK, vec![path], cx);
            }
            Some(Dialog::Confirm { action: ConfirmAction::Erase(items), .. }) => {
                self.run_job("Удаление из корзины", JobKind::NoUndo, cx, move |p, _| ops::erase_all(&items, p));
            }
            Some(Dialog::Conflict { req, apply_all }) => {
                let resolution = if req.conflict.can_merge() { Resolution::Merge } else { Resolution::Replace };
                let _ = req.reply.send(Answer { resolution, apply_to_all: apply_all });
            }
            Some(Dialog::OpenWith(ow)) => self.open_with_confirm(ow, cx),
            Some(Dialog::Password { remote, field, .. }) => {
                self.mount_remote(remote, Some(field.text), cx);
            }
            Some(Dialog::Errors { .. }) | None => {}
        }
        cx.notify();
    }

    fn answer_conflict(&mut self, resolution: Resolution, cx: &mut Context<Self>) {
        if let Some(Dialog::Conflict { req, apply_all }) = self.dialog.take() {
            let _ = req.reply.send(Answer { resolution, apply_to_all: apply_all });
        }
        cx.notify();
    }

    pub(super) fn render_dialog(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let dialog = self.dialog.as_ref()?;
        let card = match dialog {
            Dialog::Name { purpose, field, error } => {
                let (title, ok) = match purpose {
                    NamePurpose::Rename(p) => (format!("Переименовать «{}»", ops::display_name(p)), "Переименовать"),
                    NamePurpose::Create { is_dir: true, .. } => ("Новая папка".to_string(), "Создать"),
                    NamePurpose::Create { is_dir: false, .. } => ("Новый пустой файл".to_string(), "Создать"),
                };
                card(title)
                    .child(div().text_color(rgb(DIM)).child("Имя:"))
                    .child(field.render())
                    .when_some(error.clone(), |d, e| d.child(div().text_color(rgb(DANGER)).child(e)))
                    .child(buttons().child(button("cancel", "Отмена", false, cx, |this, cx| this.dialog_cancel(cx))).child(
                        button("ok", ok, true, cx, |this, cx| this.dialog_confirm(cx)),
                    ))
            }
            Dialog::Confirm { title, body, ok, .. } => card(title.clone())
                .child(div().text_color(rgb(DIM)).child(body.clone()))
                .child(
                    buttons()
                        .child(button("cancel", "Отмена", false, cx, |this, cx| this.dialog_cancel(cx)))
                        .child(button("ok", ok, true, cx, |this, cx| this.dialog_confirm(cx)).bg(rgb(DANGER)).border_color(rgb(DANGER))),
                ),
            Dialog::Conflict { req, apply_all } => {
                let c = &req.conflict;
                let parent = c.dst.parent().map(ops::display_name).unwrap_or_default();
                let name = ops::display_name(&c.dst);
                let dst_link = std::fs::symlink_metadata(&c.dst).is_ok_and(|m| m.file_type().is_symlink());
                let (head, first) = if dst_link {
                    (format!("В этой папке уже есть ссылка «{name}»."), format!("Заменить ссылку в «{parent}»"))
                } else if c.dst_is_dir {
                    (format!("В этой папке уже есть папка «{name}»."), format!("Объединить с существующей папкой в «{parent}»"))
                } else {
                    (format!("В этой папке уже есть файл «{name}»."), format!("Заменить существующий файл в «{parent}»"))
                };
                let second = if c.src_is_dir { "этой папкой?" } else { "этим файлом?" };
                let main = if c.can_merge() { "Объединить" } else { "Заменить" };
                let apply_all = *apply_all;
                card(head)
                    .child(div().child(first))
                    .child(info_box(&req.dst_info))
                    .child(div().child(second))
                    .child(info_box(&req.src_info))
                    .child(
                        div()
                            .id("apply-all")
                            .flex()
                            .items_center()
                            .gap_2()
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                if let Some(Dialog::Conflict { apply_all, .. }) = &mut this.dialog {
                                    *apply_all = !*apply_all;
                                }
                                cx.notify();
                            }))
                            .child(
                                div()
                                    .size(px(14.))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded_sm()
                                    .border_1()
                                    .border_color(rgb(if apply_all { ACCENT } else { DIM }))
                                    .when(apply_all, |d| d.bg(rgb(ACCENT)).text_color(rgb(BG)).child("✓")),
                            )
                            .child("Применить ко всем таким случаям (пробел)"),
                    )
                    .child(
                        buttons()
                            .child(button("cancel", "Отмена", false, cx, |this, cx| this.dialog_cancel(cx)))
                            .child(button("skip", "Пропустить", false, cx, |this, cx| {
                                this.answer_conflict(Resolution::Skip, cx)
                            }))
                            .child(button("rename", "Переименовать", false, cx, |this, cx| {
                                this.answer_conflict(Resolution::Rename, cx)
                            }))
                            .child(button("ok", main, true, cx, |this, cx| this.dialog_confirm(cx))),
                    )
            }
            Dialog::OpenWith(ow) => card_body()
                .w(px(520.))
                .child(self.render_open_with(ow, cx))
                .child(
                    buttons()
                        .child(button("cancel", "Отмена", false, cx, |this, cx| this.dialog_cancel(cx)))
                        .child(button("ok", "Открыть", true, cx, |this, cx| this.dialog_confirm(cx))),
                ),
            Dialog::Password { remote, field, error } => card(format!("Подключение к {}", remote.label()))
                .child(div().text_color(rgb(DIM)).child("Сервер просит пароль. Он не сохраняется."))
                .child(field.render_masked())
                .when_some(error.clone(), |d, e| d.child(div().text_color(rgb(DANGER)).child(e)))
                .child(
                    buttons()
                        .child(button("cancel", "Отмена", false, cx, |this, cx| this.dialog_cancel(cx)))
                        .child(button("ok", "Подключить", true, cx, |this, cx| this.dialog_confirm(cx))),
                ),
            Dialog::Errors { title, errors } => {
                const SHOW: usize = 50;
                let mut list = div().id("errors").flex().flex_col().gap_1().max_h(px(260.)).overflow_y_scroll();
                for (path, e) in errors.iter().take(SHOW) {
                    list = list.child(
                        div()
                            .flex()
                            .flex_col()
                            .child(div().text_color(rgb(TEXT)).child(path.display().to_string()))
                            .child(div().text_color(rgb(DANGER)).child(e.clone())),
                    );
                }
                if errors.len() > SHOW {
                    list = list.child(div().text_color(rgb(DIM)).child(format!("и ещё {}", errors.len() - SHOW)));
                }
                card(title.clone())
                    .child(list)
                    .child(buttons().child(button("ok", "Закрыть", true, cx, |this, cx| this.dialog_cancel(cx))))
            }
        };

        Some(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(hsla(0.0, 0.0, 0.0, 0.45))
                .occlude()
                .child(card)
                .into_any_element(),
        )
    }

    pub(super) fn render_menu(&self, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let menu = self.menu.as_ref()?;
        let items = &menu.items;

        // Не даём меню вылезти за край окна.
        let vp = window.viewport_size();
        let h: f32 = items.iter().map(|i| if i.is_some() { MENU_ITEM_H } else { 9.0 }).sum::<f32>() + 10.0;
        let x = f32::from(menu.pos.x).min(f32::from(vp.width) - MENU_W - 4.0).max(0.0);
        let y = f32::from(menu.pos.y).min(f32::from(vp.height) - h - 4.0).max(0.0);

        let mut list = div()
            .absolute()
            .left(px(x))
            .top(px(y))
            .w(px(MENU_W))
            .py_1()
            .flex()
            .flex_col()
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(BORDER))
            .rounded_md()
            .shadow_lg()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation());
        for (i, item) in items.iter().enumerate() {
            let Some(MenuItem { label, keys, cmd, enabled, danger }) = item else {
                list = list.child(div().my(px(4.)).h(px(1.)).bg(rgb(BORDER)));
                continue;
            };
            let (cmd, enabled, danger) = (*cmd, *enabled, *danger);
            list = list.child(
                div()
                    .id(("menu", i))
                    .flex()
                    .items_center()
                    .justify_between()
                    .h(px(MENU_ITEM_H))
                    .px_3()
                    .mx_1()
                    .rounded_sm()
                    .whitespace_nowrap()
                    .when(!enabled, |d| d.text_color(rgb(DIM)).opacity(0.6))
                    .when(enabled, |d| {
                        d.cursor_pointer()
                            .when(danger, |d| d.text_color(rgb(DANGER)))
                            .hover(|s| s.bg(rgb(SEL_BG)))
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.run_cmd(cmd, cx)))
                    })
                    .child(label.clone())
                    .child(div().text_color(rgb(DIM)).child(keys.to_string())),
            );
        }

        let close = |this: &mut Self, _: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>| {
            this.menu = None;
            cx.notify();
        };
        Some(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .occlude()
                .on_mouse_down(MouseButton::Left, cx.listener(close))
                .on_mouse_down(MouseButton::Right, cx.listener(close))
                .child(list)
                .into_any_element(),
        )
    }
}

fn card(title: impl Into<String>) -> Div {
    card_body().child(div().text_sm().font_weight(gpui::FontWeight::BOLD).child(title.into()))
}

fn card_body() -> Div {
    div()
        .w(px(480.))
        .max_w(gpui::relative(0.9))
        .p_4()
        .flex()
        .flex_col()
        .gap_3()
        .bg(rgb(PANEL))
        .border_1()
        .border_color(rgb(BORDER))
        .rounded_lg()
        .shadow_lg()
        .text_color(rgb(TEXT))
}

fn info_box(text: &str) -> Div {
    div().px_3().py_2().rounded_md().bg(rgb(BG)).text_color(rgb(DIM)).child(text.to_string())
}

fn buttons() -> Div {
    div().flex().justify_end().gap_2().pt_1()
}

fn button(
    id: &'static str,
    label: &str,
    primary: bool,
    cx: &mut Context<FilesWindow>,
    f: impl Fn(&mut FilesWindow, &mut Context<FilesWindow>) + 'static,
) -> Stateful<Div> {
    div()
        .id(id)
        .px_3()
        .py_1()
        .rounded_md()
        .border_1()
        .cursor_pointer()
        .when(primary, |d| d.bg(rgb(ACCENT)).border_color(rgb(ACCENT)).text_color(rgb(BG)))
        .when(!primary, |d| d.border_color(rgb(BORDER)).hover(|s| s.bg(rgb(BORDER))))
        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| f(this, cx)))
        .child(label.to_string())
}
