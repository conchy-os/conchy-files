//! Виды «Значки» (`Ctrl+1`) и «Компактный» (`Ctrl+3`), «Подробно» — `Ctrl+2`.
//! У картинок в виде значков — миниатюры из общего кеша freedesktop.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use conchy_files::{icons, mime, thumbs};
use gpui::{
    AnyElement, Context, MouseButton, MouseDownEvent, ObjectFit, Window, div, hsla, img, prelude::*, px, rgb,
    uniform_list,
};

use super::dnd::{DragPreview, DraggedFiles, drop_target};
use super::{DANGER, FilesWindow, SEL_BG};
use conchy_files::places::Target;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum ViewMode {
    Icons,
    #[default]
    List,
    Compact,
}

/// Миниатюры: готовые (`Some`), невозможные (`None`) и очередь на изготовление.
#[derive(Default)]
pub(super) struct Thumbs {
    done: HashMap<PathBuf, Option<PathBuf>>,
    queue: Vec<PathBuf>,
    running: bool,
}

impl Thumbs {
    /// Новая папка — старые ответы не нужны (и могли устареть).
    pub fn clear(&mut self) {
        self.done.clear();
        self.queue.clear();
    }
}

impl FilesWindow {
    pub(super) fn set_view(&mut self, mode: ViewMode, cx: &mut Context<Self>) {
        self.view_mode = mode;
        let name = match mode {
            ViewMode::Icons => "Значки",
            ViewMode::List => "Подробно",
            ViewMode::Compact => "Компактный",
        };
        self.notice(format!("Вид: {name}"), cx);
    }

    /// Размер плитки (ширина, высота) для текущего вида.
    fn tile(&self) -> (f32, f32) {
        let k = self.config.font_size / 13.0;
        match self.view_mode {
            ViewMode::Icons => ((112.0 * k).round(), (118.0 * k).round()),
            _ => ((240.0 * k).round(), (26.0 * k).round()),
        }
    }

    /// Миниатюра для картинки: из кеша или поставить в очередь.
    fn thumb(&mut self, path: &Path, mime_type: &str) -> Option<PathBuf> {
        if !thumbs::supported(mime_type) {
            return None;
        }
        match self.thumbs.done.get(path) {
            Some(t) => t.clone(),
            None => {
                if !self.thumbs.queue.iter().any(|p| p == path) {
                    self.thumbs.queue.push(path.to_path_buf());
                }
                None
            }
        }
    }

    /// Делает миниатюры из очереди в фоне, по одной пачке за раз.
    fn pump_thumbs(&mut self, cx: &mut Context<Self>) {
        if self.thumbs.running || self.thumbs.queue.is_empty() {
            return;
        }
        self.thumbs.running = true;
        let batch: Vec<PathBuf> = self.thumbs.queue.drain(..self.thumbs.queue.len().min(24)).collect();
        let job = cx.background_executor().spawn(async move {
            batch
                .into_iter()
                .map(|p| {
                    let t = thumbs::lookup(&p, thumbs::Size::Normal)
                        .or_else(|| (!thumbs::failed_before(&p)).then(|| thumbs::generate(&p, thumbs::Size::Normal).ok()).flatten());
                    (p, t)
                })
                .collect::<Vec<_>>()
        });
        cx.spawn(async move |this, cx| {
            let done = job.await;
            let _ = this.update(cx, |this, cx| {
                this.thumbs.done.extend(done);
                this.thumbs.running = false;
                this.pump_thumbs(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Сколько плиток в ряду: по ширине области списка.
    pub(super) fn grid_columns(&self, window: &Window) -> usize {
        let w = f32::from(window.viewport_size().width) - self.config.sidebar_width as f32 - 4.0;
        ((w / self.tile().0).floor() as usize).max(1)
    }

    pub(super) fn render_grid(&mut self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let cols = self.grid_columns(window);
        self.grid_cols = cols;
        let rows = self.visible.len().div_ceil(cols);
        uniform_list(
            "grid",
            rows,
            cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                let out = range
                    .map(|r| {
                        let mut line = div().flex().w_full();
                        for pos in r * cols..((r + 1) * cols).min(this.visible.len()) {
                            line = line.child(this.render_tile(pos, cx));
                        }
                        line.into_any_element()
                    })
                    .collect();
                this.pump_thumbs(cx);
                out
            }),
        )
        .track_scroll(self.list_scroll.clone())
        .size_full()
        .into_any_element()
    }

    fn render_tile(&mut self, pos: usize, cx: &mut Context<Self>) -> AnyElement {
        let ix = self.visible[pos];
        let (w, h) = self.tile();
        let icons_view = self.view_mode == ViewMode::Icons;
        let (path, mime_type, is_dir, broken, hidden, in_trash, name) = {
            let e = &self.rows[ix].entry;
            (e.path.clone(), e.mime.clone(), e.is_dir, e.broken, e.is_hidden(), e.trash.is_some(), e.name.clone())
        };
        let selected = self.selected.contains(&path);
        let cut = self.clipboard.as_ref().is_some_and(|c| c.mode == conchy_files::ops::Mode::Move && c.paths.contains(&path));
        let picture: AnyElement = if icons_view {
            let size = (64.0 * self.config.font_size / 13.0).round();
            match self.thumb(&path, &mime_type) {
                Some(t) => img(t).w(px(size)).h(px(size)).object_fit(ObjectFit::Contain).into_any_element(),
                None => {
                    let names: Vec<String> = match &self.rows[ix].icon {
                        Some(p) => vec![p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()],
                        None => mime::db().icon_names(&mime_type),
                    };
                    match icons::icons().lookup(&names, size as u32).or_else(|| self.rows[ix].icon.clone()) {
                        Some(p) => img(p).size(px(size)).into_any_element(),
                        None => div().size(px(size)).into_any_element(),
                    }
                }
            }
        } else {
            match &self.rows[ix].icon {
                Some(p) => img(p.clone()).size(px(16.)).flex_shrink_0().into_any_element(),
                None => div().size(px(16.)).into_any_element(),
            }
        };
        let dragged = if selected { self.target_paths() } else { vec![path.clone()] };
        let drop_dir = (is_dir && !in_trash).then(|| path.clone());
        let label = div()
            .overflow_hidden()
            .text_ellipsis()
            .when(icons_view, |d| d.text_center().line_clamp(2).w_full())
            .when(!icons_view, |d| d.whitespace_nowrap().flex_1().min_w_0())
            .when(broken, |d| d.text_color(rgb(DANGER)))
            .child(name);
        div()
            .id(("tile", pos))
            .w(px(w))
            .h(px(h))
            .p_1()
            .flex_shrink_0()
            .rounded_md()
            .when(icons_view, |d| d.flex().flex_col().items_center().justify_start().gap_1().pt_2())
            .when(!icons_view, |d| d.flex().items_center().gap_2().px_2())
            .when(selected, |d| d.bg(rgb(SEL_BG)))
            .when(!selected, |d| d.hover(|s| s.bg(hsla(0.0, 0.0, 1.0, 0.04))))
            .when(cut || hidden, |d| d.opacity(0.5))
            .when(!in_trash, |d| d.on_drag(DraggedFiles { paths: dragged }, |d, _, _, cx| cx.new(|_| DragPreview::new(d))))
            .when_some(drop_dir, |d, dir| drop_target(d, Target::Dir(dir), cx))
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
                    let p = this.rows[this.visible[pos]].entry.path.clone();
                    if !this.selected.contains(&p) {
                        this.click_row(pos, Default::default());
                    }
                    this.open_item_menu(ev.position, cx);
                }),
            )
            .child(picture)
            .child(label)
            .into_any_element()
    }
}
