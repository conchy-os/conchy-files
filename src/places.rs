//! Боковая панель: «Места» (домашняя папка, XDG-папки, закладки GTK) и
//! «Устройства» (смонтированные разделы со свободным местом).

use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Dir(PathBuf),
    Trash,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Place {
    pub name: String,
    pub target: Target,
    /// Имя иконки темы.
    pub icon: String,
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| "/".into())
}

fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).filter(|p| p.is_absolute()).unwrap_or_else(|| home().join(".config"))
}

/// Папки из `~/.config/user-dirs.dirs`: (ключ без `XDG_`/`_DIR`, путь).
pub fn user_dirs() -> Vec<(String, PathBuf)> {
    let Ok(s) = fs::read_to_string(config_dir().join("user-dirs.dirs")) else { return Vec::new() };
    parse_user_dirs(&s, &home())
}

fn parse_user_dirs(s: &str, home: &Path) -> Vec<(String, PathBuf)> {
    s.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .filter_map(|l| {
            let (k, v) = l.split_once('=')?;
            let key = k.trim().strip_prefix("XDG_")?.strip_suffix("_DIR")?.to_string();
            let v = v.trim().trim_matches('"');
            let path = match v.strip_prefix("$HOME") {
                Some(rest) => home.join(rest.trim_start_matches('/')),
                None => PathBuf::from(v),
            };
            // Папка, совпадающая с домашней, по спецификации значит «не задана».
            (path != home).then_some((key, path))
        })
        .collect()
}

/// Закладки GTK (`~/.config/gtk-3.0/bookmarks`) — те же, что у Thunar и Nautilus.
pub fn bookmarks() -> Vec<Place> {
    let Ok(s) = fs::read_to_string(config_dir().join("gtk-3.0/bookmarks")) else { return Vec::new() };
    parse_bookmarks(&s)
}

fn parse_bookmarks(s: &str) -> Vec<Place> {
    s.lines()
        .filter_map(|l| {
            let (uri, label) = match l.split_once(' ') {
                Some((u, n)) => (u, Some(n.trim().to_string())),
                None => (l.trim(), None),
            };
            let path = PathBuf::from(String::from_utf8_lossy(&percent_decode(uri.strip_prefix("file://")?)).into_owned());
            let name = label.filter(|n| !n.is_empty()).unwrap_or_else(|| display_name(&path));
            Some(Place { name, icon: folder_icon(&path), target: Target::Dir(path) })
        })
        .collect()
}

/// Список «Места»: как у Thunar по умолчанию, плюс закладки или XDG-папки.
/// «Файловая система» — первое из «Устройств» (см. [`mounts`]).
pub fn places() -> Vec<Place> {
    let home = home();
    let dirs = user_dirs();
    let get = |k: &str| dirs.iter().find(|(key, _)| key == k).map(|(_, p)| p.clone());
    let mut v = vec![Place { name: display_name(&home), target: Target::Dir(home.clone()), icon: "user-home".into() }];
    if let Some(d) = get("DESKTOP").filter(|d| d.is_dir()) {
        v.push(Place { name: "Рабочий стол".into(), target: Target::Dir(d), icon: "user-desktop".into() });
    }
    v.push(Place { name: "Корзина".into(), target: Target::Trash, icon: "user-trash".into() });
    let marks = bookmarks();
    if marks.is_empty() {
        // Закладок нет — показываем стандартные папки, как xdg-user-dirs-gtk делает при первом входе.
        for k in ["DOCUMENTS", "DOWNLOAD", "MUSIC", "PICTURES", "VIDEOS"] {
            if let Some(d) = get(k).filter(|d| d.is_dir()) {
                v.push(Place { name: display_name(&d), icon: folder_icon(&d), target: Target::Dir(d) });
            }
        }
    } else {
        v.extend(marks);
    }
    v
}

/// Иконка папки: для XDG-папок — своя (`folder-download`), иначе обычная.
pub fn folder_icon(path: &Path) -> String {
    if path == home() {
        return "user-home".into();
    }
    let icon = user_dirs().into_iter().find(|(_, p)| p == path).map(|(k, _)| match k.as_str() {
        "DESKTOP" => "user-desktop",
        "DOWNLOAD" => "folder-download",
        "DOCUMENTS" => "folder-documents",
        "MUSIC" => "folder-music",
        "PICTURES" => "folder-pictures",
        "VIDEOS" => "folder-videos",
        "TEMPLATES" => "folder-templates",
        "PUBLICSHARE" => "folder-publicshare",
        "PROJECTS" => "folder-projects",
        _ => "folder",
    });
    icon.unwrap_or("folder").into()
}

pub fn display_name(p: &Path) -> String {
    p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub device: String,
    pub mount_point: PathBuf,
    pub fs_type: String,
    pub name: String,
    pub total: u64,
    pub free: u64,
    pub removable: bool,
}

impl Mount {
    pub fn used_fraction(&self) -> f32 {
        if self.total == 0 { 0.0 } else { (1.0 - self.free as f64 / self.total as f64) as f32 }
    }

    pub fn icon(&self) -> &'static str {
        if self.removable { "drive-removable-media" } else { "drive-harddisk" }
    }
}

/// «Устройства»: первой идёт «Файловая система» (корень), затем разделы по правилу GIO —
/// устройства `/dev/*`, смонтированные в `/media`, `/run/media`, `/mnt` или внутри домашней папки.
pub fn mounts() -> Vec<Mount> {
    let Ok(s) = fs::read_to_string("/proc/self/mountinfo") else { return Vec::new() };
    let home = home();
    let mut out: Vec<Mount> = Vec::new();
    let all = parse_mountinfo(&s);
    // У корня берём последнюю запись: она перекрывает ранние (initramfs и т. п.).
    if let Some(root) = all.iter().rev().find(|m| m.1 == Path::new("/")) {
        let (total, free) = statvfs(Path::new("/")).unwrap_or((0, 0));
        out.push(Mount {
            device: root.0.clone(),
            mount_point: "/".into(),
            fs_type: root.2.clone(),
            name: "Файловая система".into(),
            total,
            free,
            removable: false,
        });
    }
    for m in all {
        let mp = &m.1;
        let visible = m.0.starts_with("/dev/")
            && (mp.starts_with("/media/") || mp.starts_with("/run/media/") || mp.starts_with("/mnt/")
                || (mp.starts_with(&home) && *mp != home));
        // btrfs и bind-монтирования дают одно устройство несколько раз — берём первое.
        if !visible || out.iter().any(|o| o.device == m.0) {
            continue;
        }
        let (total, free) = statvfs(mp).unwrap_or((0, 0));
        out.push(Mount {
            name: display_name(mp),
            removable: is_removable(&m.0),
            device: m.0,
            mount_point: m.1,
            fs_type: m.2,
            total,
            free,
        });
    }
    out
}

/// (устройство, точка монтирования, тип ФС) из `/proc/self/mountinfo`.
fn parse_mountinfo(s: &str) -> Vec<(String, PathBuf, String)> {
    s.lines()
        .filter_map(|l| {
            let (left, right) = l.split_once(" - ")?;
            let mp = left.split(' ').nth(4)?;
            let mut r = right.split(' ');
            let fs_type = r.next()?.to_string();
            let device = unescape_octal(r.next()?);
            Some((device, PathBuf::from(unescape_octal(mp)), fs_type))
        })
        .collect()
}

/// В mountinfo пробелы и прочее записаны как `\040`.
fn unescape_octal(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() &&b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c)) {
            out.push((b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0'));
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Съёмное ли устройство: `/sys/block/<диск>/removable` или USB в пути устройства.
fn is_removable(dev: &str) -> bool {
    let Ok(real) = fs::canonicalize(dev) else { return false };
    let Some(name) = real.file_name().and_then(|n| n.to_str()) else { return false };
    let sys = PathBuf::from("/sys/class/block").join(name);
    let disk = match fs::read_link(&sys) {
        // Для раздела (`sda1`) родитель в sysfs — сам диск (`sda`).
        Ok(link) if sys.join("partition").exists() => link.parent().and_then(|p| p.file_name()).map(|n| n.to_owned()),
        Ok(_) => Some(name.into()),
        Err(_) => None,
    };
    let Some(disk) = disk else { return false };
    let base = PathBuf::from("/sys/block").join(&disk);
    let removable = fs::read_to_string(base.join("removable")).is_ok_and(|s| s.trim() == "1");
    let usb = fs::canonicalize(&base).is_ok_and(|p| p.to_string_lossy().contains("/usb"));
    removable || usb
}

/// (всего, свободно для пользователя) в байтах.
pub fn statvfs(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let f = st.f_frsize as u64;
    Some((st.f_blocks as u64 * f, st.f_bavail as u64 * f))
}

fn percent_decode(s: &str) -> Vec<u8> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_config_files() {
        let home = Path::new("/home/u");
        let dirs = parse_user_dirs(
            "# c\nXDG_DESKTOP_DIR=\"$HOME/Рабочий стол\"\nXDG_DOWNLOAD_DIR=\"/data/dl\"\nXDG_MUSIC_DIR=\"$HOME/\"\n",
            home,
        );
        assert_eq!(dirs, vec![
            ("DESKTOP".into(), PathBuf::from("/home/u/Рабочий стол")),
            ("DOWNLOAD".into(), PathBuf::from("/data/dl")),
        ]);

        let b = parse_bookmarks("file:///home/u/%D0%9F%D1%80%D0%BE%D0%B5%D0%BA%D1%82%D1%8B\nfile:///srv/x Сервер\nsftp://host/\n");
        assert_eq!(b.len(), 2);
        assert_eq!(b[0].name, "Проекты");
        assert_eq!(b[1].name, "Сервер");
        assert_eq!(b[1].target, Target::Dir("/srv/x".into()));
    }

    #[test]
    fn parses_mountinfo() {
        let s = "36 35 0:31 / / rw - btrfs /dev/nvme0n1p2 rw\n\
                 90 36 8:17 / /run/media/u/My\\040Disk rw,nosuid - exfat /dev/sdb1 rw\n";
        let m = parse_mountinfo(s);
        assert_eq!(m[1], ("/dev/sdb1".into(), PathBuf::from("/run/media/u/My Disk"), "exfat".into()));
        assert!(statvfs(Path::new("/")).is_some_and(|(t, _)| t > 0));
    }
}
