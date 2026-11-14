//! Открытие файлов программой по умолчанию и диалог «Открыть с помощью».

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use conchy_files::apps::{self, App, Associations};
use conchy_files::{icons, mime};
use gpui::{AnyElement, ClickEvent, Context, KeyDownEvent, MouseButton, MouseDownEvent, div, img, prelude::*, px, rgb};

use super::dialogs::Dialog;
use super::field::TextField;
use super::{ACCENT, BG, BORDER, DIM, FilesWindow, SEL_BG, TEXT};

/// Установленные программы и связи типов с ними (читается в фоне при запуске).
pub(super) struct AppsIndex {
    pub all: Vec<App>,
    pub assoc: Associations,
}

impl AppsIndex {
    pub fn load() -> Self {
        AppsIndex { all: apps::all_apps(), assoc: apps::associations() }
    }

    pub fn for_mime(&self, mime: &str) -> Vec<App> {
        apps::apps_for(mime, &self.all, &self.assoc)
    }
}

pub(super) struct OpenWith {
    pub files: Vec<PathBuf>,
    pub mime: String,
    pub kind: String,
    pub recommended: Vec<App>,
    pub others: Vec<App>,
    pub filter: TextField,
    pub selected: usize,
    pub make_default: bool,
}

impl OpenWith {
    /// Видимые программы с учётом фильтра: сначала рекомендуемые, потом остальные.
    fn visible(&self) -> Vec<(&App, bool)> {
        let q = self.filter.text.to_lowercase();
        let hit = |a: &App| q.is_empty() || a.name.to_lowercase().contains(&q) || a.id.to_lowercase().contains(&q);
        self.recommended
            .iter()
            .filter(|a| hit(a))
            .map(|a| (a, true))
            .chain(self.others.iter().filter(|a| hit(a)).map(|a| (a, false)))
            .collect()
    }
}

/// Иконка программы: имя из темы или абсолютный путь из `.desktop`.
pub(super) fn app_icon(app: &App, size: u32) -> Option<PathBuf> {
    if app.icon.starts_with('/') {
        return Some(PathBuf::from(&app.icon)).filter(|p| p.is_file());
    }
    icons::icons().lookup(&[app.icon.as_str(), "application-x-executable"], size)
}

impl FilesWindow {
    pub(super) fn load_apps(&mut self, cx: &mut Context<Self>) {
        let job = cx.background_executor().spawn(async { AppsIndex::load() });
        cx.spawn(async move |this, cx| {
            let idx = job.await;
            let _ = this.update(cx, |this, _| this.apps = Some(Arc::new(idx)));
        })
        .detach();
    }

    /// Открывает файлы программами по умолчанию, по одной на группу одного типа.
    /// Для типа без программы — диалог выбора, как в Thunar.
    pub(super) fn open_files(&mut self, files: Vec<PathBuf>, cx: &mut Context<Self>) {
        let Some(idx) = self.apps.clone() else {
            // Список программ ещё читается — пусть решит xdg-open.
            for f in files {
                let _ = std::process::Command::new("xdg-open").arg(f).spawn();
            }
            return;
        };
        let mut groups: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
        for f in files {
            groups.entry(self.mime_of(&f)).or_default().push(f);
        }
        for (mime, files) in groups {
            match idx.for_mime(&mime).into_iter().next() {
                Some(app) => self.launch(&app, &files, cx),
                None => {
                    self.open_with_dialog(files, cx);
                    return;
                }
            }
        }
    }

    fn mime_of(&self, p: &Path) -> String {
        self.rows
            .iter()
            .find(|r| r.entry.path == p)
            .map(|r| r.entry.mime.clone())
            .unwrap_or_else(|| mime::db().guess(p, &conchy_files::ops::display_name(p), p.is_dir()))
    }

    pub(super) fn launch(&mut self, app: &App, files: &[PathBuf], cx: &mut Context<Self>) {
        let term = super::actions::terminal(self.config.terminal.clone());
        let cwd = self.current_dir().map(Path::to_path_buf);
        if let Err(e) = apps::launch(app, files, term.as_deref(), cwd.as_deref()) {
            self.notice(format!("Не удалось запустить «{}»: {e}", app.name), cx);
        }
    }

    /// Программы для пункта меню: подходящие ко всем выделенным файлам (без умолчания — оно в «Открыть»).
    pub(super) fn menu_apps_for(&self, files: &[PathBuf]) -> Vec<App> {
        let Some(idx) = &self.apps else { return Vec::new() };
        let mimes: Vec<String> = files.iter().map(|f| self.mime_of(f)).collect();
        let Some(first) = mimes.first() else { return Vec::new() };
        if mimes.iter().any(|m| m != first) {
            return Vec::new();
        }
        idx.for_mime(first)
    }

    pub(super) fn open_with_dialog(&mut self, files: Vec<PathBuf>, cx: &mut Context<Self>) {
        let Some(idx) = self.apps.clone() else {
            return self.notice("Список программ ещё загружается", cx);
        };
        let Some(first) = files.first() else { return };
        let mime = self.mime_of(first);
        let recommended = idx.for_mime(&mime);
        let others = idx.all.iter().filter(|a| !recommended.iter().any(|r| r.id == a.id)).cloned().collect();
        self.dialog = Some(Dialog::OpenWith(OpenWith {
            kind: mime::db().describe(&mime),
            mime,
            recommended,
            others,
            filter: TextField::new("", 0..0),
            selected: 0,
            make_default: false,
            files,
        }));
        cx.notify();
    }

    /// Клавиши диалога: стрелки выбирают, остальное — в фильтр.
    pub(super) fn open_with_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        let Some(Dialog::OpenWith(ow)) = &mut self.dialog else { return };
        let n = ow.visible().len();
        match ev.keystroke.key.as_str() {
            "down" if n > 0 => ow.selected = (ow.selected + 1).min(n - 1),
            "up" => ow.selected = ow.selected.saturating_sub(1),
            _ => {
                if ow.filter.key(&ev.keystroke) {
                    ow.selected = 0;
                }
            }
        }
        cx.notify();
    }

    /// «Открыть»: запустить выбранную программу и при желании сделать её умолчанием.
    pub(super) fn open_with_confirm(&mut self, ow: OpenWith, cx: &mut Context<Self>) {
        let Some(app) = ow.visible().get(ow.selected).map(|(a, _)| (*a).clone()) else { return };
        if ow.make_default {
            let (mime, id) = (ow.mime.clone(), app.id.clone());
            let job = cx.background_executor().spawn(async move { apps::set_default(&mime, &id).map(|_| AppsIndex::load()) });
            cx.spawn(async move |this, cx| {
                let res = job.await;
                let _ = this.update(cx, |this, cx| match res {
                    Ok(idx) => this.apps = Some(Arc::new(idx)),
                    Err(e) => this.notice(format!("Не удалось сохранить программу по умолчанию: {e}"), cx),
                });
            })
            .detach();
        }
        self.launch(&app, &ow.files, cx);
    }

    pub(super) fn render_open_with(&self, ow: &OpenWith, cx: &mut Context<Self>) -> AnyElement {
        let title = match &ow.files[..] {
            [one] => format!("Открыть «{}»", conchy_files::ops::display_name(one)),
            many => format!("Открыть {} файлов", many.len()),
        };
        let visible = ow.visible();
        let mut list = div().id("ow-list").flex().flex_col().max_h(px(300.)).overflow_y_scroll().rounded_md().bg(rgb(BG)).p_1();
        let mut last_section: Option<bool> = None;
        for (i, (app, rec)) in visible.iter().enumerate() {
            if last_section != Some(*rec) {
                last_section = Some(*rec);
                let label = if *rec { "Рекомендуемые" } else { "Другие программы" };
                list = list.child(div().px_2().pt_2().pb_1().text_size(px(11.)).text_color(rgb(DIM)).child(label));
            }
            let selected = i == ow.selected;
            list = list.child(
                div()
                    .id(("ow-app", i))
                    .flex()
                    .items_center()
                    .gap_2()
                    .h(px(30.))
                    .px_2()
                    .rounded_md()
                    .cursor_pointer()
                    .when(selected, |d| d.bg(rgb(SEL_BG)))
                    .when(!selected, |d| d.hover(|s| s.bg(rgb(BORDER))))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                            if let Some(Dialog::OpenWith(ow)) = &mut this.dialog {
                                ow.selected = i;
                            }
                            if ev.click_count >= 2 {
                                if let Some(Dialog::OpenWith(ow)) = this.dialog.take() {
                                    this.open_with_confirm(ow, cx);
                                }
                            }
                            cx.notify();
                        }),
                    )
                    .children(app_icon(app, 24).map(|p| img(p).size(px(24.)).flex_shrink_0()))
                    .child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().child(app.name.clone())),
            );
        }
        if visible.is_empty() {
            list = list.child(div().p_3().text_color(rgb(DIM)).child("Ничего не найдено"));
        }
        let make_default = ow.make_default;
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(div().text_sm().font_weight(gpui::FontWeight::BOLD).child(title))
            .child(div().text_color(rgb(DIM)).child(format!("{} ({})", ow.kind, ow.mime)))
            .child(div().flex().flex_col().gap_1().child(div().text_color(rgb(DIM)).child("Найти программу:")).child(ow.filter.render()))
            .child(list)
            .child(
                div()
                    .id("ow-default")
                    .flex()
                    .items_center()
                    .gap_2()
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        if let Some(Dialog::OpenWith(ow)) = &mut this.dialog {
                            ow.make_default = !ow.make_default;
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
                            .border_color(rgb(if make_default { ACCENT } else { DIM }))
                            .when(make_default, |d| d.bg(rgb(ACCENT)).text_color(rgb(BG)).child("✓")),
                    )
                    .child(div().text_color(rgb(TEXT)).child(format!("Всегда открывать «{}» этой программой", ow.kind))),
            )
            .into_any_element()
    }
}
