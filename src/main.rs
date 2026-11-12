mod ui;

use std::borrow::Cow;

use gpui::{App, Application, AssetSource, Bounds, SharedString, WindowBounds, WindowOptions, prelude::*, px, size};

/// Ресурсы для `svg()` — файлы иконок темы прямо с диска по абсолютному пути.
struct FsAssets;

impl AssetSource for FsAssets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        Ok(std::fs::read(path).ok().map(Cow::Owned))
    }

    fn list(&self, _path: &str) -> anyhow::Result<Vec<SharedString>> {
        Ok(Vec::new())
    }
}

fn main() {
    let start = std::env::args_os()
        .nth(1)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(conchy_files::places::home);
    let start = std::fs::canonicalize(&start).unwrap_or(start);

    // ~/.config/conchy/files.kdl; ошибку в файле покажем, но окно откроем с настройками по умолчанию.
    let settings = std::sync::Arc::new(conchy_files::settings::Settings::open());
    let config = settings.load().unwrap_or_else(|e| {
        eprintln!("conchy-files: {}: {e}", settings.path().display());
        Default::default()
    });

    Application::new().with_assets(FsAssets).run(move |cx: &mut App| {
        ui::bind_keys(cx);
        let bounds = Bounds::centered(None, size(px(config.width as f32), px(config.height as f32)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                app_id: Some("conchy-files".into()),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| ui::FilesWindow::new(start.clone(), settings.clone(), config.clone(), window, cx)),
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
