//! Диски и флешки через UDisks2 (системная шина D-Bus), без GLib.
//!
//! Тот же сервис, что у Thunar (через GIO), Nautilus и `udisksctl`: подключение
//! без root по правилам polkit, отключение, безопасное извлечение.
//! Если UDisks2 нет, «Устройства» строятся по `/proc/self/mountinfo` (см. `places`).

use std::collections::HashMap;
use std::path::PathBuf;

use zbus::blocking::Connection;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

const SERVICE: &str = "org.freedesktop.UDisks2";
const BLOCK: &str = "org.freedesktop.UDisks2.Block";
const FILESYSTEM: &str = "org.freedesktop.UDisks2.Filesystem";
const DRIVE: &str = "org.freedesktop.UDisks2.Drive";
const LOOP: &str = "org.freedesktop.UDisks2.Loop";

/// Раздел с файловой системой, который можно показать в «Устройствах».
#[derive(Debug, Clone, PartialEq)]
pub struct Volume {
    /// Объект UDisks2 (`/org/freedesktop/UDisks2/block_devices/sdb1`).
    pub object: String,
    pub device: String,
    pub label: String,
    pub fs_type: String,
    pub size: u64,
    /// Куда смонтирован (первая точка), `None` — не подключён.
    pub mount_point: Option<PathBuf>,
    /// Объект диска, на котором раздел; через него — извлечение.
    pub drive: Option<String>,
    pub removable: bool,
    pub ejectable: bool,
    pub can_power_off: bool,
    pub model: String,
    /// Системный раздел (внутренний диск): для подключения polkit спросит пароль администратора.
    pub system: bool,
    /// Образ диска, подключённый пользователем (`udisksctl loop-setup`, «Подключить образ»).
    pub is_loop: bool,
}

impl Volume {
    /// Имя для боковой панели, как у GIO: метка, иначе «Том 32 ГБ».
    pub fn name(&self) -> String {
        if !self.label.is_empty() {
            return self.label.clone();
        }
        format!("Том {}", crate::model::human_size(self.size))
    }

    pub fn icon(&self) -> &'static str {
        if self.removable { "drive-removable-media" } else { "drive-harddisk" }
    }

    /// Можно ли «безопасно извлечь» (флешка, карта, USB-диск, образ).
    pub fn can_eject(&self) -> bool {
        self.removable || self.ejectable || self.can_power_off || self.is_loop
    }
}

type Props = HashMap<String, OwnedValue>;

fn val<'a>(p: &'a Props, k: &str) -> Option<&'a OwnedValue> {
    p.get(k)
}

fn as_bool(p: &Props, k: &str) -> bool {
    val(p, k).and_then(|v| bool::try_from(v).ok()).unwrap_or(false)
}

/// Число любой беззнаковой ширины: `Size` — `t` (u64), `SetupByUID` — `u` (u32).
fn as_u64(p: &Props, k: &str) -> u64 {
    let Some(v) = val(p, k) else { return 0 };
    u64::try_from(v).ok().or_else(|| u32::try_from(v).ok().map(u64::from)).unwrap_or(0)
}

fn as_string(p: &Props, k: &str) -> String {
    val(p, k).and_then(|v| v.try_clone().ok()).and_then(|v| String::try_from(v).ok()).unwrap_or_default()
}

fn as_path(p: &Props, k: &str) -> Option<String> {
    let v = val(p, k)?.try_clone().ok()?;
    let path = OwnedObjectPath::try_from(v).ok()?;
    let s = path.as_str().to_string();
    (s != "/").then_some(s)
}

/// `ay` с нулём на конце → строка.
fn bytes_to_string(b: Vec<u8>) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

fn as_bytes_string(p: &Props, k: &str) -> String {
    val(p, k).and_then(|v| v.try_clone().ok()).and_then(|v| Vec::<u8>::try_from(v).ok()).map(bytes_to_string).unwrap_or_default()
}

fn mount_points(p: &Props) -> Vec<PathBuf> {
    val(p, "MountPoints")
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| Vec::<Vec<u8>>::try_from(v).ok())
        .unwrap_or_default()
        .into_iter()
        .map(|b| PathBuf::from(bytes_to_string(b)))
        .collect()
}

fn system_bus() -> Result<Connection, String> {
    Connection::system().map_err(|e| format!("нет системной шины D-Bus: {e}"))
}

/// Все разделы с файловыми системами, которые стоит показать пользователю, как у Thunar:
/// - без служебных (`HintIgnore`: EFI, swap, recovery);
/// - без смонтированных в системные места (`/`, `/home`, `/var`…) — корень и так
///   показан «Файловой системой»;
/// - неподключённые внутренние разделы (например, раздел Windows) и образы,
///   подключённые пользователем, — показываются, их можно подключить щелчком.
pub fn volumes() -> Result<Vec<Volume>, String> {
    let conn = system_bus()?;
    let om = zbus::blocking::fdo::ObjectManagerProxy::builder(&conn)
        .destination(SERVICE)
        .and_then(|b| b.path("/org/freedesktop/UDisks2"))
        .and_then(|b| b.build())
        .map_err(|e| format!("UDisks2 недоступен: {e}"))?;
    let objects = om.get_managed_objects().map_err(|e| format!("UDisks2 недоступен: {e}"))?;

    let drives: HashMap<String, &Props> = objects
        .iter()
        .filter_map(|(path, ifs)| ifs.iter().find(|(n, _)| n.as_str() == DRIVE).map(|(_, p)| (path.as_str().to_string(), p)))
        .collect();

    let mut out: Vec<Volume> = objects
        .iter()
        .filter_map(|(path, ifs)| {
            let block = ifs.iter().find(|(n, _)| n.as_str() == BLOCK)?.1;
            // Size 0 — пустой узел (`/dev/loop0` без образа, кардридер без карты).
            if as_string(block, "IdUsage") != "filesystem" || as_bool(block, "HintIgnore") || as_u64(block, "Size") == 0 {
                return None;
            }
            let fs = ifs.iter().find(|(n, _)| n.as_str() == FILESYSTEM).map(|(_, p)| p);
            let drive_path = as_path(block, "Drive");
            let drive = drive_path.as_ref().and_then(|d| drives.get(d)).copied();
            let removable = drive.is_some_and(|d| as_bool(d, "Removable") || as_bool(d, "MediaRemovable"));
            let system = as_bool(block, "HintSystem");
            let mount_point = fs.map(mount_points).and_then(|m| m.into_iter().next());
            if mount_point.as_ref().is_some_and(|m| !user_visible(m)) {
                return None;
            }
            // Образ, подключённый другим пользователем, — не наш.
            let lp = ifs.iter().find(|(n, _)| n.as_str() == LOOP).map(|(_, p)| p);
            let uid = unsafe { libc::getuid() } as u64;
            if lp.is_some_and(|l| as_u64(l, "SetupByUID") != uid) {
                return None;
            }
            let mut label = as_string(block, "IdLabel");
            if label.is_empty() {
                label = as_string(block, "HintName");
            }
            Some(Volume {
                object: path.as_str().to_string(),
                device: Some(as_bytes_string(block, "PreferredDevice"))
                    .filter(|d| !d.is_empty())
                    .unwrap_or_else(|| as_bytes_string(block, "Device")),
                label,
                fs_type: as_string(block, "IdType"),
                size: as_u64(block, "Size"),
                mount_point,
                drive: drive_path,
                removable,
                ejectable: drive.is_some_and(|d| as_bool(d, "Ejectable")),
                can_power_off: drive.is_some_and(|d| as_bool(d, "CanPowerOff")),
                model: drive.map(|d| format!("{} {}", as_string(d, "Vendor"), as_string(d, "Model")).trim().to_string()).unwrap_or_default(),
                system,
                is_loop: lp.is_some(),
            })
        })
        .collect();
    out.sort_by(|a, b| a.device.cmp(&b.device));
    Ok(out)
}

/// Точка монтирования, которую стоит показывать как отдельное устройство (правило GIO).
fn user_visible(m: &std::path::Path) -> bool {
    let home = crate::places::home();
    m.starts_with("/run/media/") || m.starts_with("/media/") || m.starts_with("/mnt/") || (m.starts_with(&home) && m != home)
}

fn no_options() -> HashMap<&'static str, Value<'static>> {
    HashMap::new()
}

/// Текст ошибки UDisks2 без служебного префикса D-Bus.
fn clean(e: zbus::Error) -> String {
    let s = e.to_string();
    let s = s.split_once(": ").map(|(_, rest)| rest.to_string()).unwrap_or(s);
    if s.contains("NotAuthorized") || s.contains("not authorized") {
        return "нет разрешения (polkit)".into();
    }
    if s.contains("target is busy") || s.contains("DeviceBusy") {
        return "устройство занято: закройте открытые на нём файлы и программы".into();
    }
    s
}

/// Подключает раздел, возвращает точку монтирования (`/run/media/<user>/<метка>`).
pub fn mount(volume: &Volume) -> Result<PathBuf, String> {
    let conn = system_bus()?;
    let reply = conn
        .call_method(Some(SERVICE), volume.object.as_str(), Some(FILESYSTEM), "Mount", &(no_options(),))
        .map_err(clean)?;
    reply.body().deserialize::<String>().map(PathBuf::from).map_err(|e| e.to_string())
}

pub fn unmount(volume: &Volume) -> Result<(), String> {
    let conn = system_bus()?;
    conn.call_method(Some(SERVICE), volume.object.as_str(), Some(FILESYSTEM), "Unmount", &(no_options(),))
        .map_err(clean)?;
    Ok(())
}

/// «Безопасно извлечь»: отключает все разделы этого диска, затем извлекает
/// носитель или обесточивает USB-устройство — после этого его можно вынимать.
pub fn eject(volume: &Volume, all: &[Volume]) -> Result<(), String> {
    if volume.is_loop {
        // Образ: отключить и отсоединить, как «Извлечь» в GIO.
        if volume.mount_point.is_some() {
            unmount(volume)?;
        }
        let conn = system_bus()?;
        conn.call_method(Some(SERVICE), volume.object.as_str(), Some(LOOP), "Delete", &(no_options(),))
            .map_err(clean)?;
        return Ok(());
    }
    let Some(drive) = &volume.drive else { return unmount(volume) };
    for v in all.iter().filter(|v| v.drive.as_ref() == Some(drive) && v.mount_point.is_some()) {
        unmount(v)?;
    }
    let conn = system_bus()?;
    let method = if volume.can_power_off { "PowerOff" } else { "Eject" };
    if volume.can_power_off || volume.ejectable {
        conn.call_method(Some(SERVICE), drive.as_str(), Some(DRIVE), method, &(no_options(),))
            .map_err(clean)?;
    }
    Ok(())
}

/// Следит за UDisks2 (устройство подключили, вынули, смонтировали) и вызывает `on_change`.
/// Поток живёт, пока жив процесс; без UDisks2 сразу завершается.
pub fn watch(on_change: impl Fn() + Send + 'static) {
    std::thread::Builder::new()
        .name("conchy-files-udisks".into())
        .spawn(move || {
            let Ok(conn) = Connection::system() else { return };
            let Ok(rule) = zbus::MatchRule::builder().msg_type(zbus::message::Type::Signal).sender(SERVICE).map(|b| b.build()) else {
                return;
            };
            let Ok(iter) = zbus::blocking::MessageIterator::for_match_rule(rule, &conn, Some(64)) else { return };
            for msg in iter {
                if msg.is_err() {
                    break;
                }
                on_change();
            }
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vol(label: &str, size: u64) -> Volume {
        Volume {
            object: String::new(),
            device: "/dev/sdb1".into(),
            label: label.into(),
            fs_type: "vfat".into(),
            size,
            mount_point: None,
            drive: None,
            removable: true,
            ejectable: false,
            can_power_off: true,
            model: String::new(),
            system: false,
            is_loop: false,
        }
    }

    #[test]
    fn names_like_gio() {
        assert_eq!(vol("ФЛЕШКА", 0).name(), "ФЛЕШКА");
        assert_eq!(vol("", 32 << 30).name(), "Том 32.0 ГБ");
        assert!(vol("", 0).can_eject());
        assert_eq!(bytes_to_string(b"/dev/sdb1\0".to_vec()), "/dev/sdb1");
        assert!(user_visible(std::path::Path::new("/run/media/u/USB")));
        assert!(!user_visible(std::path::Path::new("/")) && !user_visible(std::path::Path::new("/var/log")));
    }

    #[test]
    fn lists_volumes_if_udisks_is_running() {
        // На машине без UDisks2 (CI) просто проверяем, что ошибка, а не паника.
        match volumes() {
            Ok(v) => assert!(v.iter().all(|v| !v.object.is_empty())),
            Err(e) => assert!(!e.is_empty()),
        }
    }
}
