//! Окно «Открыть с помощью» для портала AppChooser: небольшой список приложений из `choices`.
//! Отдельное окно (`app_id = conchy-files-appchooser`), без файлового списка; значки и
//! `.desktop`-индекс те же, что у диалога «Открыть с помощью» в самом файловом менеджере.
//!
//! «Всегда открывать так» здесь нет: запоминание делает сам портал через permission store.

use std::sync::Arc;
use std::time::Instant;

use conchy_files::mime;
use conchy_files::pick as logic;
use conchy_portal_proto::apps::{AppChooseRequest, AppChooseResult};
use conchy_portal_proto::response;
use futures::channel::oneshot;
use gpui::{
    App, AnyWindowHandle, ClickEvent, Context, FocusHandle, KeyDownEvent, MouseButton, MouseDownEvent, Window, div, img,
    prelude::*, px, rgb,
};

use super::field::TextField;
use super::openwith::{AppsIndex, app_icon};
use super::{ACCENT, BG, BORDER, DIM, PANEL, SEL_BG, TEXT};

pub struct AppChooserWindow {
    focus: FocusHandle,
    id: String,
    req: AppChooseRequest,
    choices: Vec<String>,
    reply: Option<oneshot::Sender<(u32, AppChooseResult)>>,
    window: AnyWindowHandle,
    apps: Option<Arc<AppsIndex>>,
    filter: TextField,
    selected: usize,
    started: Instant,
    first_frame_logged: bool,
}

impl AppChooserWindow {
    fn new(id: String, req: AppChooseRequest, reply: oneshot::Sender<(u32, AppChooseResult)>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        focus.focus(window);
        // Индекс приложений читается в фоне; окно появляется сразу.
        let job = cx.background_executor().spawn(async { AppsIndex::load() });
        cx.spawn(async move |this, cx| {
            let idx = job.await;
            let _ = this.update(cx, |this, cx| {
                this.apps = Some(Arc::new(idx));
                cx.notify();
            });
        })
        .detach();
        cx.on_release(|this, _| this.abandon()).detach();
        Self {
            focus,
            id,
            choices: req.choices.clone(),
            req,
            reply: Some(reply),
            window: window.window_handle(),
            apps: None,
            filter: TextField::new("", 0..0),
            selected: 0,
            started: Instant::now(),
            first_frame_logged: false,
        }
    }

    fn visible(&self) -> Vec<&conchy_files::apps::App> {
        match &self.apps {
            Some(idx) => logic::chooser_apps(&idx.all, &self.choices, &self.req.last_choice, &self.filter.text),
            None => Vec::new(),
        }
    }

    fn finish(&mut self, code: u32, choice: Option<String>, cx: &mut Context<Self>) {
        let Some(reply) = self.reply.take() else { return };
        let result = AppChooseResult { choice: choice.unwrap_or_default(), activation_token: String::new() };
        let _ = reply.send((code, result));
        let handle = self.window;
        cx.defer(move |cx| {
            let _ = handle.update(cx, |_, window, _| window.remove_window());
        });
    }

    fn accept(&mut self, cx: &mut Context<Self>) {
        let chosen = self
            .visible()
            .get(self.selected)
            .map(|a| a.id.strip_suffix(".desktop").unwrap_or(&a.id).to_string());
        match chosen {
            Some(c) => self.finish(response::OK, Some(c), cx),
            None => cx.notify(),
        }
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        self.finish(response::CANCELLED, None, cx);
    }

    /// Окно ушло без ответа — демон получает «отмена».
    fn abandon(&mut self) {
        if let Some(reply) = self.reply.take() {
            let _ = reply.send((response::CANCELLED, AppChooseResult::default()));
        }
    }

    fn on_key(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let n = self.visible().len();
        match ev.keystroke.key.as_str() {
            "escape" => return self.cancel(cx),
            "enter" => return self.accept(cx),
            "down" if n > 0 => self.selected = (self.selected + 1).min(n - 1),
            "up" => self.selected = self.selected.saturating_sub(1),
            _ => {
                if self.filter.key(&ev.keystroke) {
                    self.selected = 0;
                }
            }
        }
        cx.stop_propagation();
        cx.notify();
    }
}

impl Render for AppChooserWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.first_frame_logged {
            self.first_frame_logged = true;
            if std::env::var_os("CONCHY_PORTAL_TRACE").is_some() {
                eprintln!("[conchy-files] app chooser first frame: {:.1} ms", self.started.elapsed().as_secs_f64() * 1000.0);
            }
        }
        window.set_window_title("Открыть с помощью");
        let subject = if !self.req.filename.is_empty() {
            self.req.filename.clone()
        } else {
            self.req.uri.clone()
        };
        let kind = (!self.req.content_type.is_empty()).then(|| mime::db().describe(&self.req.content_type));
        let visible = self.visible();
        let mut list = div().id("ac-list").flex().flex_col().flex_1().min_h_0().overflow_y_scroll().rounded_md().bg(rgb(BG)).p_1();
        for (i, app) in visible.iter().enumerate() {
            let selected = i == self.selected;
            list = list.child(
                div()
                    .id(("ac-app", i))
                    .flex()
                    .items_center()
                    .gap_2()
                    .h(px(34.))
                    .px_2()
                    .flex_shrink_0()
                    .rounded_md()
                    .cursor_pointer()
                    .when(selected, |d| d.bg(rgb(SEL_BG)))
                    .when(!selected, |d| d.hover(|s| s.bg(rgb(BORDER))))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                            this.selected = i;
                            if ev.click_count >= 2 {
                                this.accept(cx);
                            }
                            cx.notify();
                        }),
                    )
                    .children(app_icon(app, 24).map(|p| img(p).size(px(24.)).flex_shrink_0()))
                    .child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().child(app.name.clone())),
            );
        }
        if visible.is_empty() {
            let text = if self.apps.is_none() { "Загрузка…" } else { "Ничего не найдено" };
            list = list.child(div().p_3().text_color(rgb(DIM)).child(text));
        }
        let btn = |id: &'static str, label: &str, primary: bool| {
            div()
                .id(id)
                .px_4()
                .py_1()
                .rounded_md()
                .border_1()
                .cursor_pointer()
                .when(primary, |d| d.bg(rgb(ACCENT)).border_color(rgb(ACCENT)).text_color(rgb(BG)))
                .when(!primary, |d| d.border_color(rgb(BORDER)).hover(|s| s.bg(rgb(BORDER))))
                .child(label.to_string())
        };
        div()
            .key_context("AppChooser")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .size_full()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .bg(rgb(PANEL))
            .text_color(rgb(TEXT))
            .child(div().text_sm().font_weight(gpui::FontWeight::BOLD).child(if subject.is_empty() {
                "Выберите приложение".to_string()
            } else {
                format!("Открыть «{subject}»")
            }))
            .children(kind.map(|k| div().text_color(rgb(DIM)).child(k)))
            .child(self.filter.render())
            .child(list)
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(btn("ac-cancel", "Отмена", false).on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.cancel(cx))))
                    .child(btn("ac-ok", "Выбрать", true).on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.accept(cx)))),
            )
    }
}

/// Открывает окно выбора приложения.
pub fn open_app_chooser(cx: &mut App, id: String, req: AppChooseRequest, reply: oneshot::Sender<(u32, AppChooseResult)>) {
    let bounds = gpui::Bounds::centered(None, gpui::size(px(420.), px(520.)), cx);
    let handle = cx.open_window(
        gpui::WindowOptions {
            window_bounds: Some(gpui::WindowBounds::Windowed(bounds)),
            app_id: Some("conchy-files-appchooser".into()),
            ..Default::default()
        },
        move |window, cx| cx.new(|cx| AppChooserWindow::new(id, req, reply, window, cx)),
    );
    if let Ok(handle) = handle {
        let _ = handle.update(cx, |_, window, _| window.activate_window());
    }
}

/// `UpdateChoices` от фронтенда: заменить список в открытом окне.
pub fn update_app_choices(cx: &mut App, id: &str, choices: Vec<String>) {
    for w in cx.windows() {
        if let Some(h) = w.downcast::<AppChooserWindow>() {
            let choices = choices.clone();
            let _ = h.update(cx, |this, _, cx| {
                if this.id == id {
                    this.choices = choices;
                    this.selected = 0;
                    cx.notify();
                }
            });
        }
    }
}

/// `Request.Close` от фронтенда.
pub fn cancel_app_chooser(cx: &mut App, id: &str) {
    for w in cx.windows() {
        if let Some(h) = w.downcast::<AppChooserWindow>() {
            let _ = h.update(cx, |this, _, cx| {
                if this.id == id {
                    this.cancel(cx);
                }
            });
        }
    }
}
