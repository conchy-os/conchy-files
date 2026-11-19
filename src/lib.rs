pub mod analysis;
pub mod apps;
pub mod cache;
pub mod devices;
pub mod dir;
pub mod fm1;
pub mod icons;
pub mod instance;
pub mod layout;
pub mod mime;
pub mod model;
pub mod notify;
pub mod ops;
pub mod pick;
pub mod portal_host;
pub mod places;
pub mod remote;
pub mod scan;
pub mod search;
pub mod settings;
pub mod trash;
pub mod thumbs;
pub mod treemap;
pub mod uca;
pub mod watch;

/// `%D0%A4` → байты. Нераспознанные `%` остаются как есть.
pub fn percent_decode(s: &str) -> Vec<u8> {
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push(h << 4 | l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

/// Тесты работают с отдельной корзиной во временной папке, а не с настоящей.
/// Переменная ставится один раз до запуска любого теста, который трогает корзину.
#[cfg(test)]
pub(crate) fn test_xdg() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let d = std::env::temp_dir().join(format!("conchy-files-xdg-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        unsafe { std::env::set_var("XDG_DATA_HOME", d) };
    });
}

/// Тесты миниатюр пишут в свой кеш, а не в `~/.cache/thumbnails`.
#[cfg(test)]
pub(crate) fn test_xdg_cache() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let d = std::env::temp_dir().join(format!("conchy-files-cache-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        unsafe { std::env::set_var("XDG_CACHE_HOME", d) };
    });
}
