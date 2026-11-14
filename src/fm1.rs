//! `org.freedesktop.FileManager1` — стандартный интерфейс «Показать в папке».
//!
//! Им пользуются браузеры (загрузки → «Показать в папке»), Telegram, IDE и т. д.
//! Сервис регистрирует только основной экземпляр. Если имя уже занято
//! (запущен Thunar или Nautilus), conchy-files просто работает без него.

use zbus::interface;

use crate::instance::{Request, arg_to_path};

pub const NAME: &str = "org.freedesktop.FileManager1";
pub const PATH: &str = "/org/freedesktop/FileManager1";

pub struct FileManager1 {
    on_request: Box<dyn Fn(Vec<Request>) + Send + Sync>,
}

fn paths(uris: Vec<String>) -> Vec<std::path::PathBuf> {
    uris.iter().filter_map(|u| arg_to_path(u)).filter(|p| p.is_absolute()).collect()
}

#[interface(name = "org.freedesktop.FileManager1")]
impl FileManager1 {
    /// Открыть папки.
    fn show_folders(&self, uris: Vec<String>, _startup_id: String) {
        (self.on_request)(paths(uris).into_iter().map(Request::Open).collect());
    }

    /// Открыть папки, где лежат элементы, и выделить их.
    fn show_items(&self, uris: Vec<String>, _startup_id: String) {
        (self.on_request)(paths(uris).into_iter().map(Request::Select).collect());
    }

    /// Показать свойства элементов.
    fn show_item_properties(&self, uris: Vec<String>, _startup_id: String) {
        (self.on_request)(paths(uris).into_iter().map(Request::Properties).collect());
    }
}

/// Регистрирует сервис на сессионной шине. Соединение надо держать живым.
pub fn serve(on_request: impl Fn(Vec<Request>) + Send + Sync + 'static) -> zbus::Result<zbus::blocking::Connection> {
    zbus::blocking::connection::Builder::session()?
        .name(NAME)?
        .serve_at(PATH, FileManager1 { on_request: Box::new(on_request) })?
        .build()
}
