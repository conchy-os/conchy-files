mod ui;

use std::borrow::Cow;
use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};
use std::path::PathBuf;
use std::sync::Arc;

use conchy_files::instance::{self, Request};
use conchy_files::portal_host::HostEvent;
use conchy_files::settings::Settings;
use futures::StreamExt;
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

const HELP: &str = "conchy-files — файловый менеджер Conchy OS

Использование: conchy-files [ПАРАМЕТРЫ] [ПАПКА|ФАЙЛ|file://URI]…

  ПАПКА         открыть в новом окне (по умолчанию — домашняя)
  ФАЙЛ          открыть его папку и выделить его
  --select ПУТЬ показать элемент в его папке
  --service     запуск через D-Bus (org.freedesktop.FileManager1): без окна,
                ждать запроса; без запроса выйти через 30 секунд
  --resident    резидентный режим для порталов (org.conchy.Portal.FilesHost): остаётся после
                закрытия окон и держит прогретые кеши, для этого висит невидимое окно
                `conchy-files-keepalive` (нужно правило wm: плавающее, без фокуса); выходит,
                если 15 минут нет окон и запросов (CONCHY_FILES_IDLE_SECS).
                Без него порталы поднимают conchy-files через D-Bus (--service) на каждый запрос.
  -h, --help    эта справка

Если программа уже запущена, новое окно открывает работающий процесс.
Когда закрыто последнее окно, процесс завершается и освобождает память
(кроме режима --resident).
Настройки: ~/.config/conchy/files.kdl";

fn main() {
    let mut service = false;
    let mut resident = false;
    let mut reqs = Vec::new();
    let mut args = std::env::args_os().skip(1);
    while let Some(a) = args.next() {
        let s = a.to_string_lossy();
        match s.as_ref() {
            "--service" => service = true,
            "--resident" => resident = true,
            "-h" | "--help" => return println!("{HELP}"),
            "--select" => {
                if let Some(p) = args.next().and_then(|p| instance::arg_to_path(&p.to_string_lossy())) {
                    reqs.push(Request::Select(absolute(p)));
                }
            }
            // Путь как есть (в имени может быть не-UTF-8), URI — раскодировать.
            _ if !s.contains("://") => reqs.push(Request::Open(absolute(PathBuf::from(&a)))),
            _ => match instance::arg_to_path(&s) {
                Some(p) => reqs.push(Request::Open(absolute(p))),
                None => eprintln!("conchy-files: {s}: сетевые адреса открывайте через строку пути (Ctrl+L)"),
            },
        }
    }
    if reqs.is_empty() && !service && !resident {
        reqs.push(Request::Open(conchy_files::places::home()));
    }

    // Уже запущены — передаём, что открыть, и выходим.
    let socket = instance::socket_path();
    if instance::send_to_running(&socket, &reqs).unwrap_or(false) {
        return;
    }
    let listener = match instance::listen(&socket) {
        Ok(l) => Some(l),
        Err(e) => {
            eprintln!("conchy-files: {}: {e}; работаю без одиночного экземпляра", socket.display());
            None
        }
    };

    let settings = Arc::new(Settings::open());
    if let Err(e) = settings.load() {
        eprintln!("conchy-files: {}: {e}", settings.path().display());
    }

    // Хост UI для демона порталов. Имя занимаем ДО инициализации GPUI (она долгая): демон ждёт
    // активации не больше 2 секунд, а запросы пока копятся в канале.
    let (host_tx, host_rx) = futures::channel::mpsc::unbounded::<HostEvent>();
    match conchy_files::portal_host::serve(host_tx) {
        Ok(conn) => std::mem::forget(conn),
        // Имя занято (другой экземпляр conchy-files) — пикеры откроет он.
        Err(e) => eprintln!("conchy-files: FilesHost не запущен: {e}"),
    }
    let mut host_rx = host_rx;

    Application::new().with_assets(FsAssets).run(move |cx: &mut App| {
        ui::bind_keys(cx);

        // Запросы от следующих запусков и от FileManager1 приходят в одну очередь.
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<Vec<Request>>();
        if let Some(l) = listener {
            let tx = tx.clone();
            instance::serve(l, move |r| {
                let _ = tx.unbounded_send(r);
            });
        }
        let fm = {
            let tx = tx.clone();
            conchy_files::fm1::serve(move |r| {
                let _ = tx.unbounded_send(r);
            })
        };
        // Соединение с шиной живёт, пока жив процесс.
        if let Ok(conn) = fm {
            std::mem::forget(conn);
        }
        let activity = Rc::new(Cell::new(Instant::now()));
        {
            let s = settings.clone();
            let activity = activity.clone();
            cx.spawn(async move |cx| {
                while let Some(ev) = host_rx.next().await {
                    activity.set(Instant::now());
                    let s = s.clone();
                    let _ = cx.update(|cx| match ev {
                        HostEvent::Pick { id, req, reply } => ui::open_picker(cx, id, *req, reply, &s),
                        HostEvent::Cancel { id } => ui::cancel_pick(cx, &id),
                    });
                }
            })
            .detach();
        }

        for r in reqs.drain(..) {
            open_window(cx, r, &settings);
        }
        let s = settings.clone();
        cx.spawn(async move |cx| {
            while let Some(batch) = rx.next().await {
                for r in batch {
                    let s = s.clone();
                    let _ = cx.update(|cx| open_window(cx, r, &s));
                }
            }
        })
        .detach();

        // Последнее окно закрыто — выходим. (GPUI под Wayland и сам останавливает
        // цикл событий без окон, поэтому «вечного» фонового режима нет.)
        cx.on_window_closed(move |cx| {
            if cx.windows().is_empty() && !resident {
                cx.quit();
            }
        })
        .detach();
        if resident {
            // GPUI на Wayland останавливает цикл событий, когда не осталось ни одного окна
            // (gpui/src/platform/linux/wayland/client.rs), а нам нужен тёплый процесс. Держим
            // крошечное прозрачное окно; wm даёт ему правило по app_id `conchy-files-keepalive`.
            let _ = cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(1.), px(1.)), cx))),
                    app_id: Some("conchy-files-keepalive".into()),
                    titlebar: None,
                    focus: false,
                    show: false,
                    is_movable: false,
                    is_resizable: false,
                    is_minimizable: false,
                    window_background: gpui::WindowBackgroundAppearance::Transparent,
                    ..Default::default()
                },
                |_, cx| cx.new(|_| KeepAlive),
            );
            // Освобождаем память, если порталы давно не нужны: следующий запрос запустит процесс заново.
            let idle = std::env::var("CONCHY_FILES_IDLE_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(900u64);
            let activity = activity.clone();
            cx.spawn(async move |cx| {
                loop {
                    gpui::Timer::after(Duration::from_secs(idle.clamp(1, 60))).await;
                    let quit = cx
                        .update(|cx| {
                            cx.windows().len() <= 1 && activity.get().elapsed() >= Duration::from_secs(idle)
                        })
                        .unwrap_or(true);
                    if quit {
                        let _ = cx.update(|cx| cx.quit());
                        break;
                    }
                }
            })
            .detach();
        }
        if service {
            // Запустили ради запроса D-Bus, а он не пришёл (или уже обработан другим) — не висим.
            cx.spawn(async move |cx| {
                gpui::Timer::after(std::time::Duration::from_secs(30)).await;
                let _ = cx.update(|cx| {
                    if cx.windows().is_empty() {
                        cx.quit();
                    }
                });
            })
            .detach();
        }
        cx.activate(true);
    });
    // Ответ D-Bus уходит из отдельного потока zbus: даём ему долететь до выхода.
    std::thread::sleep(Duration::from_millis(100));
    let _ = std::fs::remove_file(&socket);
}

/// Пустое невидимое окно-якорь резидентного режима.
struct KeepAlive;

impl Render for KeepAlive {
    fn render(&mut self, _: &mut gpui::Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        gpui::div().size_full()
    }
}

fn absolute(p: PathBuf) -> PathBuf {
    let p = if p.is_absolute() { p } else { std::env::current_dir().map(|d| d.join(&p)).unwrap_or(p) };
    std::fs::canonicalize(&p).unwrap_or(p)
}

fn open_window(cx: &mut App, req: Request, settings: &Arc<Settings>) {
    // Каждое окно берёт свежие настройки: их могли поменять, пока процесс жил в фоне.
    let config = settings.load().unwrap_or_default();
    let bounds = Bounds::centered(None, size(px(config.width as f32), px(config.height as f32)), cx);
    let select = matches!(req, Request::Select(_) | Request::Properties(_));
    let props = matches!(req, Request::Properties(_));
    let path = req.path().to_path_buf();
    let settings = settings.clone();
    let handle = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            app_id: Some("conchy-files".into()),
            ..Default::default()
        },
        |window, cx| cx.new(|cx| ui::FilesWindow::new(path, select, settings, config, window, cx)),
    );
    if let Ok(handle) = handle {
        let _ = handle.update(cx, |view, window, cx| {
            window.activate_window();
            if props {
                view.show_panel(cx);
            }
        });
    }
}
