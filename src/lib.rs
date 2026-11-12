pub mod dir;
pub mod icons;
pub mod layout;
pub mod mime;
pub mod model;
pub mod ops;
pub mod places;
pub mod scan;
pub mod settings;
pub mod trash;
pub mod treemap;
pub mod watch;

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
