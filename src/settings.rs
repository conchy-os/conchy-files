//! Настройки из единого KDL-конфига Conchy OS: канал `files` (`~/.config/conchy/files.kdl`).
//!
//! Схема и разбор живут в `conchy-config`, как у остальных компонентов.
//! - Чтение: файл канала напрямую (демон для этого не нужен).
//! - Запись: через демон `org.conchy.Config` (`Set`), чтобы другие программы получили
//!   `PropertyChanged`; если демона нет — сами пишем файл, сохраняя комментарии.
//! - Изменения: inotify на каталоге настроек. Так ловятся и правки руками в редакторе,
//!   и `conchy-config set files …`, и записи демона по D-Bus.

use std::path::{Path, PathBuf};

use conchy_config::ConfigStore;
pub use conchy_config::FilesConfig;
pub use conchy_config::schema::files::{FolderSize, SortBy, default_files_kdl, parse_files_config};

use crate::watch::{self, Watch};

pub const CHANNEL: &str = "files";

pub struct Settings {
    store: ConfigStore,
    /// Писать через D-Bus. В тестах выключено, чтобы не трогать настоящий демон.
    use_bus: bool,
}

impl Settings {
    /// `$XDG_CONFIG_HOME/conchy` (обычно `~/.config/conchy`).
    pub fn open() -> Self {
        Settings { store: ConfigStore::open(None), use_bus: true }
    }

    /// Отдельный каталог (тесты, `--config`).
    pub fn in_dir(dir: PathBuf) -> Self {
        Settings { store: ConfigStore::open(Some(dir)), use_bus: false }
    }

    pub fn path(&self) -> PathBuf {
        self.store.channel_path(CHANNEL)
    }

    pub fn dir(&self) -> &Path {
        self.store.base_dir()
    }

    /// Читает настройки. При первом запуске создаёт `files.kdl` с комментариями,
    /// чтобы было что править. Чужие каналы (`wm`, `session`) не создаются.
    pub fn load(&self) -> Result<FilesConfig, String> {
        // Каталог только для чтения — не беда, работаем со значениями по умолчанию.
        let _ = self.store.ensure_channel(CHANNEL, default_files_kdl());
        let doc = self.store.load_document(CHANNEL).map_err(|e| format!("{e:#}"))?;
        parse_files_config(&doc).map_err(|e| format!("{e:#}"))
    }

    /// Пользовательские действия из блока `actions` (см. [`crate::uca`]).
    pub fn actions(&self) -> Vec<crate::uca::CustomAction> {
        self.store.load_document(CHANNEL).map(|d| crate::uca::parse(&d)).unwrap_or_default()
    }

    /// Меняет одно значение (`view.show-hidden` = `true`). Вызывать не из потока интерфейса:
    /// обращение к D-Bus может ждать.
    pub fn set(&self, key: &str, value: &str) -> Result<(), String> {
        if self.use_bus && set_via_bus(key, value).is_ok() {
            return Ok(());
        }
        self.store.set(CHANNEL, key, value).map_err(|e| format!("{e:#}"))
    }

    /// Следит за каталогом настроек и вызывает `on_change` после правок.
    pub fn watch(&self, on_change: impl Fn() + Send + 'static) -> std::io::Result<Watch> {
        std::fs::create_dir_all(self.dir())?;
        watch::watch(&[self.dir()], on_change)
    }
}

fn set_via_bus(key: &str, value: &str) -> zbus::Result<()> {
    let conn = zbus::blocking::Connection::session()?;
    let proxy = conchy_config::bus::ConfigProxyBlocking::new(&conn)?;
    proxy.set(CHANNEL, key, value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_reads_and_writes_own_channel() {
        let dir = std::env::temp_dir().join(format!("conchy-settings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let s = Settings::in_dir(dir.clone());

        assert_eq!(s.load().unwrap(), FilesConfig::default());
        assert!(s.path().exists(), "files.kdl создан при первом запуске");
        assert!(!dir.join("wm.kdl").exists(), "чужие каналы не трогаем");

        s.set("view.show-hidden", "true").unwrap();
        s.set("view.sort-by", "size").unwrap();
        let c = s.load().unwrap();
        assert!(c.show_hidden);
        assert_eq!(c.sort_by, SortBy::Size);
        assert!(std::fs::read_to_string(s.path()).unwrap().contains("// Ctrl+H"), "комментарии на месте");

        std::fs::write(s.path(), "view {\n  show-hidden \n").unwrap();
        assert!(s.load().is_err(), "синтаксическая ошибка видна");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
