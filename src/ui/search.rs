//! Поиск `Ctrl+F`: строка пути становится полем поиска, находки появляются
//! в списке по мере обхода. Колонка «Тип» показывает, где лежит найденное.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use conchy_files::dir::Entry;
use conchy_files::search;
use futures::StreamExt;
use gpui::{AnyElement, ClickEvent, Context, KeyDownEvent, Task, Timer, div, prelude::*, px, rgb};

use super::field::TextField;
use super::{BORDER, DIM, FilesWindow, Row, items_label};

pub(super) struct SearchState {
    pub root: PathBuf,
    pub field: TextField,
    /// Курсор в поле поиска (иначе — в списке находок).
    pub editing: bool,
    pub running: bool,
    pub limited: bool,
    cancel: Arc<AtomicBool>,
    _task: Option<Task<()>>,
}

impl SearchState {
    /// Останавливает фоновый обход (ушли из папки, закрыли поиск).
    pub(super) fn stop(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl FilesWindow {
    pub(super) fn start_search(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.current_dir().map(Path::to_path_buf) else {
            return self.notice("В корзине поиск не работает", cx);
        };
        match &mut self.search {
            Some(s) => s.editing = true,
            None => {
                self.location_edit = None;
                self.search = Some(SearchState {
                    root,
                    field: TextField::new("", 0..0),
                    editing: true,
                    running: false,
                    limited: false,
                    cancel: search::cancel_flag(),
                    _task: None,
                });
            }
        }
        cx.notify();
    }

    /// Выход из поиска: снова содержимое папки.
    pub(super) fn exit_search(&mut self, cx: &mut Context<Self>) {
        if let Some(s) = self.search.take() {
            s.stop();
            self.reload(cx);
        }
        cx.notify();
    }

    pub(super) fn on_search_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        cx.stop_propagation();
        let Some(s) = &mut self.search else { return };
        match ev.keystroke.key.as_str() {
            "escape" => return self.exit_search(cx),
            // Enter или стрелка вниз — к находкам: дальше работают обычные клавиши списка.
            "enter" | "down" => {
                s.editing = false;
                if self.selected.is_empty() && !self.visible.is_empty() {
                    self.click_row(0, Default::default());
                }
            }
            _ => {
                let before = s.field.text.clone();
                s.field.key(&ev.keystroke);
                if s.field.text != before {
                    self.schedule_search(cx);
                }
            }
        }
        cx.notify();
    }

    /// Запуск поиска через 250 мс после последней нажатой клавиши.
    fn schedule_search(&mut self, cx: &mut Context<Self>) {
        let Some(s) = &mut self.search else { return };
        s.cancel.store(true, Ordering::Relaxed);
        let task = cx.spawn(async move |this, cx| {
            Timer::after(Duration::from_millis(250)).await;
            let _ = this.update(cx, |this, cx| this.run_search(cx));
        });
        s._task = Some(task);
    }

    fn run_search(&mut self, cx: &mut Context<Self>) {
        let hidden = self.show_hidden;
        let special = self.special_icons.clone();
        let Some(s) = &mut self.search else { return };
        s.cancel.store(true, Ordering::Relaxed);
        let cancel = search::cancel_flag();
        s.cancel = cancel.clone();
        s.limited = false;
        let query = s.field.text.clone();
        let root = s.root.clone();
        self.rows.clear();
        self.visible.clear();
        self.selected.clear();
        self.cursor = None;
        if query.trim().is_empty() {
            s.running = false;
            cx.notify();
            return;
        }
        s.running = true;

        // Поток обхода → пачки путей → строки списка (в фоне) → окно.
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<Vec<Row>>();
        let (done_tx, done_rx) = futures::channel::oneshot::channel::<bool>();
        let c = cancel.clone();
        std::thread::spawn(move || {
            let r = root.clone();
            let limited = search::find(&root, &query, hidden, &c, |batch| {
                let rows = batch
                    .into_iter()
                    .filter_map(|p| Entry::read(&p, false).ok())
                    .map(|e| {
                        let (icon, _) = super::icon_and_kind(&e, &special);
                        // Вместо типа — где лежит, относительно папки поиска.
                        let kind = e.path.parent().and_then(|p| p.strip_prefix(&r).ok()).map(|p| p.display().to_string()).unwrap_or_default();
                        let kind = if kind.is_empty() { "здесь".into() } else { kind };
                        super::make_row(e, icon, kind)
                    })
                    .collect();
                let _ = tx.unbounded_send(rows);
            });
            let _ = done_tx.send(limited);
        });
        let task = cx.spawn(async move |this, cx| {
            while let Some(rows) = rx.next().await {
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                let alive = this.update(cx, |this, cx| {
                    this.rows.extend(rows);
                    this.rebuild_visible();
                    cx.notify();
                });
                if alive.is_err() {
                    return;
                }
            }
            let limited = done_rx.await.unwrap_or(false);
            let _ = this.update(cx, |this, cx| {
                if let Some(s) = &mut this.search {
                    if !cancel.load(Ordering::Relaxed) {
                        s.running = false;
                        s.limited = limited;
                    }
                }
                cx.notify();
            });
        });
        if let Some(s) = &mut self.search {
            s._task = Some(task);
        }
        cx.notify();
    }

    /// Поле поиска на месте строки пути.
    pub(super) fn render_search_field(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let s = self.search.as_ref()?;
        let place = conchy_files::places::display_name(&s.root);
        let status = if s.running {
            "ищу…".to_string()
        } else if s.field.text.trim().is_empty() {
            format!("в «{place}» и вложенных папках")
        } else if s.limited {
            format!("первые {} — уточните запрос", search::LIMIT)
        } else {
            format!("найдено: {}", items_label(self.rows.len() as u64))
        };
        Some(
            div()
                .flex()
                .flex_1()
                .min_w_0()
                .items_center()
                .gap_2()
                .child(div().text_color(rgb(DIM)).child("Поиск:"))
                .child(
                    div()
                        .id("search-field")
                        .flex_1()
                        .min_w_0()
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            if let Some(s) = &mut this.search {
                                s.editing = true;
                            }
                            cx.notify();
                        }))
                        .child(s.field.render()),
                )
                .child(div().flex_shrink_0().text_color(rgb(DIM)).text_size(px(12.)).child(status))
                .child(
                    div()
                        .id("search-close")
                        .px_2()
                        .rounded_md()
                        .cursor_pointer()
                        .hover(|st| st.bg(rgb(BORDER)))
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.exit_search(cx)))
                        .child("✕ Esc"),
                )
                .into_any_element(),
        )
    }
}
