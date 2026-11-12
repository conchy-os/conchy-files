//! Отрисовка частей окна: панель инструментов с путём, боковая панель,
//! список (вид «Подробно») и строка состояния.

use std::path::{Path, PathBuf};

use conchy_files::dir::SortKey;
use conchy_files::icons;
use conchy_files::model::human_size;
use conchy_files::ops::Mode;
use conchy_files::places::{self, Target};
use gpui::{
    AnyElement, ClickEvent, Context, Hsla, MouseButton, MouseDownEvent, SharedString, div, hsla, img,
    prelude::*, px, rgb, svg, uniform_list,
};

use super::{ACCENT, BORDER, DIM, FilesWindow, GoBack, GoForward, GoHome, GoUp, PANEL, SEL_BG, TEXT, items_label};

// Размеры при шрифте 13 px; растут вместе с `theme.font-size` из files.kdl.
const BASE_FONT: f32 = 13.0;
const ROW_H: f32 = 24.0;
const COL_SIZE: f32 = 96.0;
const COL_TYPE: f32 = 180.0;
const COL_DATE: f32 = 150.0;

/// Иконка из темы по пути или пустое место того же размера.
fn icon(path: Option<&PathBuf>, size: f32) -> AnyElement {
    match path {
        Some(p) => img(p.clone()).size(px(size)).flex_shrink_0().into_any_element(),
        None => div().size(px(size)).flex_shrink_0().into_any_element(),
    }
}

/// Одноцветная иконка кнопки (`go-previous` → `go-previous-symbolic.svg`), окрашенная в `color`.
fn symbolic(name: &str, fallback: &'static str, color: impl Into<Hsla>) -> AnyElement {
    let color = color.into();
    match icons::symbolic(name) {
        Some(p) => svg().path(SharedString::from(p.display().to_string())).size(px(16.)).flex_shrink_0().text_color(color).into_any_element(),
        None => div().text_color(color).child(fallback).into_any_element(),
    }
}

impl FilesWindow {
    /// Размер, пересчитанный под шрифт из настроек.
    fn scaled(&self, v: f32) -> f32 {
        (v * self.config.font_size / BASE_FONT).round()
    }

    // ---------- панель инструментов ----------

    pub(super) fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let parent = self.current_dir().and_then(Path::parent).is_some();
        let nav = |id: &'static str, icon_name: &str, fb: &'static str, enabled: bool, tip: &'static str| {
            div()
                .id(id)
                .size(px(28.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_md()
                .flex_shrink_0()
                .when(enabled, |d| d.cursor_pointer().hover(|s| s.bg(rgb(BORDER))))
                .child(symbolic(icon_name, fb, rgb(if enabled { TEXT } else { BORDER })))
                .tooltip(tooltip(tip))
        };
        div()
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .h(px(40.))
            .flex_shrink_0()
            .bg(rgb(PANEL))
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(nav("back", "go-previous", "‹", !self.back.is_empty(), "Назад (Alt+←)").on_click(
                cx.listener(|this, _: &ClickEvent, w, cx| this.go_back(&GoBack, w, cx)),
            ))
            .child(nav("forward", "go-next", "›", !self.forward.is_empty(), "Вперёд (Alt+→)").on_click(
                cx.listener(|this, _: &ClickEvent, w, cx| this.go_forward(&GoForward, w, cx)),
            ))
            .child(nav("up", "go-up", "↑", parent, "Вверх (Alt+↑)").on_click(
                cx.listener(|this, _: &ClickEvent, w, cx| this.go_up(&GoUp, w, cx)),
            ))
            .child(nav("home", "go-home", "⌂", true, "Домашняя папка (Alt+Home)").on_click(
                cx.listener(|this, _: &ClickEvent, w, cx| this.go_home(&GoHome, w, cx)),
            ))
            .child(div().w(px(6.)))
            .child(self.render_location(cx))
    }

    /// Путь кнопками, как в Thunar; по `Ctrl+L` или щелчку по пустому месту — поле ввода.
    fn render_location(&self, cx: &mut Context<Self>) -> AnyElement {
        if let Some(field) = &self.location_edit {
            return div().flex_1().min_w_0().child(field.render()).into_any_element();
        }
        let mut crumbs = div().flex().items_center().gap_0p5().min_w_0().overflow_hidden();
        match &self.location {
            Target::Trash => {
                crumbs = crumbs.child(crumb("crumb-trash", "user-trash", "Корзина".into(), true, cx, |_, _| {}));
            }
            Target::Dir(cur) => {
                let home = places::home();
                // Внутри домашней папки путь начинается с неё, как в Thunar.
                let start: PathBuf = if cur.starts_with(&home) { home.clone() } else { "/".into() };
                let mut parts: Vec<PathBuf> = cur.ancestors().take_while(|a| a.starts_with(&start)).map(Path::to_path_buf).collect();
                parts.reverse();
                for (i, p) in parts.into_iter().enumerate() {
                    if i > 0 {
                        crumbs = crumbs.child(div().text_color(rgb(DIM)).px_0p5().child("›"));
                    }
                    let current = p == *cur;
                    let (icon_name, label) = if p == home {
                        ("user-home", places::display_name(&p))
                    } else if p.parent().is_none() {
                        ("drive-harddisk", "Файловая система".to_string())
                    } else {
                        ("", places::display_name(&p))
                    };
                    let target = p.clone();
                    crumbs = crumbs.child(crumb(("crumb", i), icon_name, label, current, cx, move |this, cx| {
                        this.navigate(Target::Dir(target.clone()), cx)
                    }));
                }
            }
        }
        div()
            .id("location")
            .flex()
            .flex_1()
            .min_w_0()
            .items_center()
            .h(px(28.))
            .cursor_text()
            .on_click(cx.listener(|this, ev: &ClickEvent, _, cx| {
                if ev.click_count() >= 1 {
                    this.start_location_edit(cx);
                }
            }))
            .child(crumbs)
            .into_any_element()
    }

    // ---------- боковая панель ----------

    pub(super) fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let heading = |t: &'static str| div().px_3().pt_3().pb_1().text_color(rgb(DIM)).text_size(px(11.)).child(t);
        let mut col = div()
            .id("sidebar")
            .flex()
            .flex_col()
            .w(px(self.config.sidebar_width as f32))
            .flex_shrink_0()
            .overflow_y_scroll()
            .bg(rgb(PANEL))
            .border_r_1()
            .border_color(rgb(BORDER))
            .pb_2()
            .child(heading("Места"));

        for (i, place) in self.places.iter().enumerate() {
            let active = place.target == self.location;
            let target = place.target.clone();
            let icon_path = icons::icons().lookup(&[place.icon.as_str(), "folder"], 16);
            col = col.child(
                side_row(("place", i), active)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.navigate(target.clone(), cx)))
                    .child(icon(icon_path.as_ref(), 16.))
                    .child(div().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().child(place.name.clone())),
            );
        }

        if !self.mounts.is_empty() {
            col = col.child(heading("Устройства"));
            for (i, m) in self.mounts.iter().enumerate() {
                let target = Target::Dir(m.mount_point.clone());
                // Подсвечиваем самое длинное подходящее место: внутри флешки — флешку, а не корень.
                let best = self.current_dir().and_then(|c| {
                    self.mounts.iter().filter(|x| c.starts_with(&x.mount_point)).max_by_key(|x| x.mount_point.as_os_str().len())
                });
                let in_place = self.places.iter().any(|p| p.target == self.location);
                let active = !in_place && best.is_some_and(|b| b.mount_point == m.mount_point);
                let icon_path = icons::icons().lookup(&[m.icon(), "drive-harddisk"], 16);
                let used = m.used_fraction();
                let tip = format!(
                    "{} · {} · свободно {} из {}",
                    m.device,
                    m.fs_type,
                    human_size(m.free),
                    human_size(m.total)
                );
                col = col.child(
                    side_row(("mount", i), active)
                        .h(px(36.))
                        .tooltip(tooltip(tip))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.navigate(target.clone(), cx)))
                        .child(icon(icon_path.as_ref(), 16.))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .flex_1()
                                .min_w_0()
                                .gap(px(3.))
                                .child(div().overflow_hidden().text_ellipsis().whitespace_nowrap().child(m.name.clone()))
                                .child(usage_bar(used)),
                        ),
                );
            }
        }
        col
    }

    // ---------- список ----------

    pub(super) fn render_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let trash = self.in_trash();
        let header_cell = |id: &'static str, label: &'static str, key: SortKey, w: Option<f32>, right: bool, cx: &mut Context<Self>| {
            let arrow = if self.sort.key == key { if self.sort.ascending { " ▴" } else { " ▾" } } else { "" };
            let d = div()
                .id(id)
                .flex()
                .items_center()
                .h_full()
                .px_2()
                .cursor_pointer()
                .whitespace_nowrap()
                .hover(|s| s.bg(rgb(BORDER)))
                .when(right, |d| d.justify_end())
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.resort(key, cx)))
                .child(format!("{label}{arrow}"));
            match w {
                Some(w) => d.w(px(w)).flex_shrink_0(),
                None => d.flex_1().min_w_0(),
            }
        };
        let head = div()
            .flex()
            .h(px(self.scaled(ROW_H)))
            .flex_shrink_0()
            .text_color(rgb(DIM))
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(header_cell("h-name", "Имя", SortKey::Name, None, false, cx))
            .child(header_cell("h-size", "Размер", SortKey::Size, Some(self.scaled(COL_SIZE)), true, cx))
            .child(header_cell("h-type", if trash { "Исходное место" } else { "Тип" }, SortKey::Type, Some(self.scaled(COL_TYPE)), false, cx))
            .child(header_cell("h-date", if trash { "Удалён" } else { "Изменён" }, SortKey::Modified, Some(self.scaled(COL_DATE)), false, cx));

        let body: AnyElement = if let Some(err) = &self.load_error {
            placeholder(err.clone())
        } else if self.visible.is_empty() && !self.loading {
            let hidden = self.rows.len();
            placeholder(if hidden > 0 {
                format!("Только скрытые файлы ({hidden}) — Ctrl+H, чтобы показать")
            } else if trash {
                "Корзина пуста".to_string()
            } else {
                "Папка пуста".to_string()
            })
        } else {
            uniform_list(
                "files",
                self.visible.len(),
                cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                    let cut: Vec<&Path> = this
                        .clipboard
                        .as_ref()
                        .filter(|c| c.mode == Mode::Move)
                        .map(|c| c.paths.iter().map(PathBuf::as_path).collect())
                        .unwrap_or_default();
                    range.map(|pos| this.render_row(pos, &cut, cx)).collect()
                }),
            )
            .track_scroll(self.list_scroll.clone())
            .size_full()
            .into_any_element()
        };

        div()
            .id("list")
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            // Нажатие на пустое место снимает выделение, правая кнопка — меню папки.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _, cx| {
                    this.selected.clear();
                    this.menu = None;
                    this.location_edit = None;
                    cx.notify();
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                    this.selected.clear();
                    this.open_folder_menu(ev.position, cx);
                }),
            )
            .child(head)
            .child(div().flex_1().min_h_0().child(body))
    }

    fn render_row(&self, pos: usize, cut: &[&Path], cx: &mut Context<Self>) -> AnyElement {
        let row = &self.rows[self.visible[pos]];
        let e = &row.entry;
        let selected = self.selected.contains(&e.path);
        let is_cursor = self.cursor.as_ref() == Some(&e.path);
        let dim = cut.contains(&e.path.as_path()) || e.is_hidden();
        div()
            .id(("row", pos))
            .flex()
            .items_center()
            .h(px(self.scaled(ROW_H)))
            .w_full()
            .when(selected, |d| d.bg(rgb(SEL_BG)))
            .when(!selected, |d| d.hover(|s| s.bg(hsla(0.0, 0.0, 1.0, 0.04))))
            .when(is_cursor && !selected, |d| d.border_1().border_color(rgb(BORDER)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    if ev.click_count >= 2 && !ev.modifiers.control && !ev.modifiers.shift {
                        let ix = this.visible[pos];
                        this.open_row(ix, cx);
                    } else {
                        this.click_row(pos, ev.modifiers);
                    }
                    cx.notify();
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    let path = this.rows[this.visible[pos]].entry.path.clone();
                    if !this.selected.contains(&path) {
                        this.click_row(pos, Default::default());
                    }
                    this.open_item_menu(ev.position, cx);
                }),
            )
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .when(dim, |d| d.opacity(0.5))
                    .child(icon(row.icon.as_ref(), 16.))
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .when(e.is_symlink, |d| d.italic())
                            .when(e.broken, |d| d.text_color(rgb(super::DANGER)))
                            .child(e.name.clone()),
                    ),
            )
            .child(cell(self.scaled(COL_SIZE)).justify_end().text_color(rgb(DIM)).child(row.size.clone()))
            .child(cell(self.scaled(COL_TYPE)).text_color(rgb(DIM)).child(row.kind.clone()))
            .child(cell(self.scaled(COL_DATE)).text_color(rgb(DIM)).child(row.date.clone()))
            .into_any_element()
    }

    // ---------- строка состояния ----------

    pub(super) fn render_status(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let sel = self.selected_rows();
        let text = if self.loading && self.rows.is_empty() {
            "Загрузка…".to_string()
        } else if sel.len() == 1 {
            // Как Thunar: «имя» (размер) тип.
            let r = &self.rows[sel[0]];
            let e = &r.entry;
            if e.is_dir {
                let n = e.items.map(items_label).unwrap_or_default();
                format!("«{}»  {}  ·  {n}", e.name, r.kind)
            } else if let Some(t) = &e.trash {
                format!("«{}» ({})  ·  было в {}", e.name, human_size(e.size), t.original.parent().map(|p| p.display().to_string()).unwrap_or_default())
            } else {
                let mut s = format!("«{}» ({})  {}", e.name, human_size(e.size), r.kind);
                if e.is_symlink {
                    if let Ok(t) = std::fs::read_link(&e.path) {
                        s.push_str(&format!("  →  {}", t.display()));
                    }
                }
                s
            }
        } else if sel.len() > 1 {
            let size: u64 = sel.iter().map(|&i| self.rows[i].entry.size).sum();
            format!("Выбрано: {}  ({})", items_label(sel.len() as u64), human_size(size))
        } else {
            let size: u64 = self.visible.iter().map(|&i| self.rows[i].entry.size).sum();
            let mut s = format!("{}  ({})", items_label(self.visible.len() as u64), human_size(size));
            if let Some((_, free)) = self.space {
                s.push_str(&format!(",  свободно: {}", human_size(free)));
            }
            s
        };

        let right = if let Some(job) = &self.job {
            let p = &job.progress;
            let frac = p.fraction();
            let current = p.current.lock().map(|c| conchy_files::ops::display_name(&c)).unwrap_or_default();
            Some(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .flex_shrink_0()
                    .text_color(rgb(TEXT))
                    .child(format!("{}… {:.0}%", job.title, frac * 100.0))
                    .child(progress_bar(frac, 120.))
                    .child(div().max_w(px(200.)).overflow_hidden().text_ellipsis().text_color(rgb(DIM)).child(current))
                    .child(
                        div()
                            .id("cancel-job")
                            .px_1()
                            .rounded_sm()
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(BORDER)))
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.cancel_job();
                                cx.notify();
                            }))
                            .child("✕ Esc"),
                    )
                    .into_any_element(),
            )
        } else {
            self.current_notice().map(|n| div().flex_shrink_0().text_color(rgb(ACCENT)).child(n.to_string()).into_any_element())
        };

        div()
            .h(px(24.))
            .px_3()
            .flex()
            .items_center()
            .justify_between()
            .gap_4()
            .flex_shrink_0()
            .bg(rgb(PANEL))
            .border_t_1()
            .border_color(rgb(BORDER))
            .text_color(rgb(DIM))
            .whitespace_nowrap()
            .child(div().min_w_0().overflow_hidden().text_ellipsis().child(text))
            .children(right)
    }
}

/// Всплывающая подсказка с текстом.
struct Tip(SharedString);

impl Render for Tip {
    fn render(&mut self, _: &mut gpui::Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(BORDER))
            .rounded_md()
            .text_size(px(12.))
            .text_color(rgb(TEXT))
            .child(self.0.clone())
    }
}

fn tooltip(text: impl Into<SharedString>) -> impl Fn(&mut gpui::Window, &mut gpui::App) -> gpui::AnyView + 'static {
    let text = text.into();
    move |_, cx| cx.new(|_| Tip(text.clone())).into()
}

fn crumb(
    id: impl Into<gpui::ElementId>,
    icon_name: &str,
    label: String,
    current: bool,
    cx: &mut Context<FilesWindow>,
    on: impl Fn(&mut FilesWindow, &mut Context<FilesWindow>) + 'static,
) -> AnyElement {
    let icon_path = (!icon_name.is_empty()).then(|| icons::icons().lookup(&[icon_name], 16)).flatten();
    div()
        .id(id)
        .flex()
        .items_center()
        .gap_1()
        .h(px(26.))
        .px_2()
        .rounded_md()
        .whitespace_nowrap()
        .flex_shrink_0()
        .cursor_pointer()
        .when(current, |d| d.bg(rgb(BORDER)).text_color(rgb(TEXT)))
        .when(!current, |d| d.text_color(rgb(DIM)).hover(|s| s.bg(rgb(BORDER))))
        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
            cx.stop_propagation();
            on(this, cx);
        }))
        .children(icon_path.map(|p| icon(Some(&p), 16.)))
        .child(label)
        .into_any_element()
}

fn side_row(id: impl Into<gpui::ElementId>, active: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap_2()
        .h(px(28.))
        .mx_1()
        .px_2()
        .rounded_md()
        .cursor_pointer()
        .when(active, |d| d.bg(rgb(SEL_BG)))
        .when(!active, |d| d.hover(|s| s.bg(rgb(BORDER))))
}

/// Занятое место на диске: синяя полоска, ближе к заполнению — красная.
fn usage_bar(used: f32) -> impl IntoElement {
    let color = if used > 0.9 { rgb(super::DANGER) } else { rgb(ACCENT) };
    div()
        .relative()
        .w_full()
        .h(px(4.))
        .rounded_sm()
        .bg(rgb(BORDER))
        .child(div().absolute().top_0().left_0().h_full().rounded_sm().bg(color).w(gpui::relative(used.clamp(0.0, 1.0))))
}

fn progress_bar(frac: f32, w: f32) -> impl IntoElement {
    div()
        .relative()
        .w(px(w))
        .h(px(6.))
        .rounded_sm()
        .bg(rgb(BORDER))
        .child(div().absolute().top_0().left_0().h_full().rounded_sm().bg(rgb(ACCENT)).w(px(w * frac.clamp(0.0, 1.0))))
}

fn cell(w: f32) -> gpui::Div {
    div().w(px(w)).flex_shrink_0().flex().items_center().px_2().overflow_hidden().whitespace_nowrap().text_ellipsis()
}

fn placeholder(text: String) -> AnyElement {
    div().size_full().flex().items_center().justify_center().text_color(rgb(DIM)).child(text).into_any_element()
}
