//! Уведомления рабочего стола: `org.freedesktop.Notifications` на сессионной шине.
//!
//! В Conchy OS их показывает conchy-shell (поддерживает кнопки-действия), в других
//! окружениях — любой сервер уведомлений. Сокет conchy-shell не используется:
//! незнакомая строка там открывает лаунчер.

use std::collections::HashMap;

use zbus::zvariant::Value;

/// Отправляет уведомление. `false` — сервера уведомлений нет, показать надо в окне.
/// Обращается к D-Bus: вызывать не из потока интерфейса.
pub fn send(summary: &str, body: &str, icon: &str) -> bool {
    let Ok(conn) = zbus::blocking::Connection::session() else { return false };
    let mut hints: HashMap<&str, Value> = HashMap::new();
    hints.insert("desktop-entry", Value::from("conchy-files"));
    let actions: Vec<&str> = Vec::new();
    conn.call_method(
        Some("org.freedesktop.Notifications"),
        "/org/freedesktop/Notifications",
        Some("org.freedesktop.Notifications"),
        "Notify",
        &("Файлы", 0u32, icon, summary, body, actions, hints, -1i32),
    )
    .is_ok()
}
