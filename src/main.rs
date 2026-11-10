mod ui;

use gpui::{App, Application, Bounds, WindowBounds, WindowOptions, prelude::*, px, size};

fn main() {
    let root = std::env::args_os()
        .nth(1)
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(Into::into))
        .unwrap_or_else(|| "/".into());
    let root = std::fs::canonicalize(&root).unwrap_or(root);

    Application::new().run(move |cx: &mut App| {
        ui::bind_keys(cx);
        let bounds = Bounds::centered(None, size(px(1280.), px(820.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                app_id: Some("conchy-files".into()),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| ui::FilesView::new(root.clone(), window, cx)),
        )
        .unwrap();
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
    });
}
