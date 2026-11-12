//! Корзина по спецификации freedesktop.org (Trash 1.0).
//!
//! Та же корзина, что у Thunar, Nautilus и `gio trash`: файлы, удалённые здесь,
//! видны в `trash:///` других файловых менеджеров, и наоборот.
//!
//! - Файлы с домашнего раздела идут в `$XDG_DATA_HOME/Trash` (обычно `~/.local/share/Trash`).
//! - Файлы с других разделов идут в `$topdir/.Trash/$uid` (если админ создал `.Trash`
//!   с битом sticky) или в `$topdir/.Trash-$uid`. Через границу разделов файлы
//!   никогда не копируются: перенос в корзину всегда мгновенный `rename`.

use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Файл, который лежит в корзине. Хватает, чтобы вернуть его на место.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrashedItem {
    /// Где файл был до удаления.
    pub original: PathBuf,
    /// Где он лежит сейчас: `…/Trash/files/<имя>`.
    pub file: PathBuf,
    /// Его описание: `…/Trash/info/<имя>.trashinfo`.
    pub info: PathBuf,
}

/// Переносит `path` в корзину. `path` должен быть абсолютным.
pub fn trash(path: &Path) -> io::Result<TrashedItem> {
    let meta = fs::symlink_metadata(path)?;
    let path = absolute_no_follow(path)?;
    if path.parent().is_none() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "корень нельзя удалить в корзину"));
    }

    let home_trash = home_trash_dir()?;
    let (trash_dir, topdir) = if same_device(&home_trash, meta.dev()) {
        (home_trash, None)
    } else {
        let top = mount_point(&path, meta.dev())?;
        (topdir_trash(&top)?, Some(top))
    };

    let files = trash_dir.join("files");
    let info = trash_dir.join("info");
    create_dir_private(&trash_dir)?;
    create_dir_private(&files)?;
    create_dir_private(&info)?;

    // Для корзины раздела путь в .trashinfo пишется относительно его корня.
    let stored = match &topdir {
        Some(top) => path.strip_prefix(top).unwrap_or(&path).to_path_buf(),
        None => path.clone(),
    };
    let body = format!(
        "[Trash Info]\nPath={}\nDeletionDate={}\n",
        percent_encode(stored.as_os_str().as_bytes()),
        local_timestamp()
    );

    let base = path.file_name().unwrap().to_os_string();
    // Спецификация требует сначала атомарно занять имя .trashinfo (O_EXCL),
    // и только потом переносить файл: так две программы не займут одно имя.
    for n in 1u32.. {
        let name = if n == 1 { base.clone() } else { numbered(&base, n) };
        let info_path = info.join(with_suffix(&name, ".trashinfo"));
        let mut f = match OpenOptions::new().write(true).create_new(true).open(&info_path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };
        let file_path = files.join(&name);
        if fs::symlink_metadata(&file_path).is_ok() {
            // Мусор от чужой программы: .trashinfo нет, а файл есть. Берём следующее имя.
            drop(f);
            let _ = fs::remove_file(&info_path);
            continue;
        }
        if let Err(e) = f.write_all(body.as_bytes()).and_then(|_| f.sync_all()) {
            let _ = fs::remove_file(&info_path);
            return Err(e);
        }
        if let Err(e) = fs::rename(&path, &file_path) {
            let _ = fs::remove_file(&info_path);
            return Err(e);
        }
        return Ok(TrashedItem { original: path, file: file_path, info: info_path });
    }
    unreachable!()
}

/// Возвращает файл из корзины на прежнее место. Если там уже что-то есть, ничего не трогает
/// и возвращает ошибку `AlreadyExists`.
pub fn restore(item: &TrashedItem) -> io::Result<()> {
    if fs::symlink_metadata(&item.original).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} уже существует", item.original.display()),
        ));
    }
    if let Some(parent) = item.original.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(&item.file, &item.original)?;
    let _ = fs::remove_file(&item.info);
    Ok(())
}

/// Запись в корзине с датой удаления (секунды Unix, 0 — неизвестна).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub item: TrashedItem,
    pub deleted: i64,
}

/// Всё содержимое корзин: домашней и корзин разделов `topdirs`
/// (обычно точки монтирования из «Устройств»).
pub fn list(topdirs: &[PathBuf]) -> Vec<Listed> {
    let uid = unsafe { libc::geteuid() };
    let mut dirs: Vec<(PathBuf, Option<PathBuf>)> = Vec::new();
    if let Ok(h) = home_trash_dir() {
        dirs.push((h, None));
    }
    for top in topdirs {
        dirs.push((top.join(".Trash").join(uid.to_string()), Some(top.clone())));
        dirs.push((top.join(format!(".Trash-{uid}")), Some(top.clone())));
    }
    let mut out = Vec::new();
    for (dir, top) in dirs {
        let Ok(entries) = fs::read_dir(dir.join("info")) else { continue };
        for e in entries.flatten() {
            let info = e.path();
            let Some(stem) = info.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_suffix(".trashinfo")) else {
                continue;
            };
            let file = dir.join("files").join(stem);
            if fs::symlink_metadata(&file).is_err() {
                continue;
            }
            let Ok(body) = fs::read_to_string(&info) else { continue };
            let Some((path, deleted)) = parse_info(&body) else { continue };
            let original = match &top {
                Some(t) if path.is_relative() => t.join(path),
                _ => path,
            };
            out.push(Listed { item: TrashedItem { original, file, info }, deleted });
        }
    }
    out
}

/// Удаляет из корзины насовсем.
pub fn erase(item: &TrashedItem) -> io::Result<()> {
    let meta = fs::symlink_metadata(&item.file)?;
    if meta.is_dir() {
        fs::remove_dir_all(&item.file)?;
    } else {
        fs::remove_file(&item.file)?;
    }
    let _ = fs::remove_file(&item.info);
    Ok(())
}

fn parse_info(body: &str) -> Option<(PathBuf, i64)> {
    use std::os::unix::ffi::OsStringExt;
    let mut path = None;
    let mut deleted = 0;
    for line in body.lines() {
        if let Some(p) = line.strip_prefix("Path=") {
            path = Some(PathBuf::from(OsString::from_vec(percent_decode(p))));
        } else if let Some(d) = line.strip_prefix("DeletionDate=") {
            deleted = parse_local_time(d).unwrap_or(0);
        }
    }
    Some((path?, deleted))
}

/// `2026-11-12T10:41:05` в местном времени → секунды Unix.
fn parse_local_time(s: &str) -> Option<i64> {
    let (date, time) = s.split_once('T')?;
    let d: Vec<i32> = date.split('-').map(|x| x.parse().ok()).collect::<Option<_>>()?;
    let t: Vec<i32> = time.split(':').map(|x| x.parse().ok()).collect::<Option<_>>()?;
    if d.len() != 3 || t.len() < 2 {
        return None;
    }
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    tm.tm_year = d[0] - 1900;
    tm.tm_mon = d[1] - 1;
    tm.tm_mday = d[2];
    tm.tm_hour = t[0];
    tm.tm_min = t[1];
    tm.tm_sec = *t.get(2).unwrap_or(&0);
    tm.tm_isdst = -1;
    let r = unsafe { libc::mktime(&mut tm) };
    (r != -1).then_some(r as i64)
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

fn home_trash_dir() -> io::Result<PathBuf> {
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "не задан HOME"))?;
    Ok(data.join("Trash"))
}

/// Лежит ли `dir` (или ближайший существующий родитель) на устройстве `dev`.
fn same_device(dir: &Path, dev: u64) -> bool {
    let mut cur = Some(dir);
    while let Some(p) = cur {
        if let Ok(m) = fs::metadata(p) {
            return m.dev() == dev;
        }
        cur = p.parent();
    }
    false
}

/// Корень раздела, на котором лежит `path`: поднимаемся, пока не сменится устройство.
fn mount_point(path: &Path, dev: u64) -> io::Result<PathBuf> {
    let mut top = path.parent().unwrap_or(path).to_path_buf();
    while let Some(parent) = top.parent() {
        if fs::metadata(parent)?.dev() != dev {
            break;
        }
        top = parent.to_path_buf();
    }
    Ok(top)
}

/// Корзина раздела: `$topdir/.Trash/$uid`, если `.Trash` настроен админом правильно,
/// иначе `$topdir/.Trash-$uid`.
fn topdir_trash(top: &Path) -> io::Result<PathBuf> {
    let uid = unsafe { libc::geteuid() };
    let shared = top.join(".Trash");
    if let Ok(m) = fs::symlink_metadata(&shared) {
        let sticky = m.permissions().mode() & 0o1000 != 0;
        if m.is_dir() && sticky {
            return Ok(shared.join(uid.to_string()));
        }
    }
    Ok(top.join(format!(".Trash-{uid}")))
}

fn create_dir_private(dir: &Path) -> io::Result<()> {
    match fs::create_dir(dir) {
        Ok(()) => fs::set_permissions(dir, fs::Permissions::from_mode(0o700)),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

/// Абсолютный путь без разыменования последнего компонента (сам симлинк, а не цель).
fn absolute_no_follow(path: &Path) -> io::Result<PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "у пути нет имени"))?;
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => fs::canonicalize(p)?,
        _ => std::env::current_dir()?,
    };
    Ok(parent.join(name))
}

fn numbered(name: &std::ffi::OsStr, n: u32) -> OsString {
    let mut s = name.to_os_string();
    s.push(format!(".{n}"));
    s
}

fn with_suffix(name: &std::ffi::OsStr, suffix: &str) -> OsString {
    let mut s = name.to_os_string();
    s.push(suffix);
    s
}

/// Кодирование пути для `Path=` в .trashinfo (как в URL, но `/` остаётся).
fn percent_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Местное время в формате `2026-11-12T10:41:05`, как требует спецификация.
fn local_timestamp() -> String {
    let tm = local_tm(unsafe { libc::time(std::ptr::null_mut()) } as i64);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

/// Разбор времени Unix в местный часовой пояс.
pub(crate) fn local_tm(t: i64) -> libc::tm {
    unsafe {
        let t = t as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        tm
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_paths() {
        assert_eq!(percent_encode("/home/u/Мои файлы/a b.txt".as_bytes()),
            "/home/u/%D0%9C%D0%BE%D0%B8%20%D1%84%D0%B0%D0%B9%D0%BB%D1%8B/a%20b.txt");
    }

    #[test]
    fn trash_and_restore_roundtrip() {
        // Отдельная корзина в /tmp, чтобы тест не трогал настоящую.
        crate::test_xdg();
        let base = std::env::temp_dir().join(format!("conchy-trash-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("data/sub")).unwrap();
        fs::write(base.join("data/sub/f.txt"), b"hello").unwrap();
        fs::write(base.join("data/f.txt"), b"other").unwrap();

        let a = trash(&base.join("data/sub/f.txt")).unwrap();
        let b = trash(&base.join("data/f.txt")).unwrap();
        assert!(!base.join("data/f.txt").exists());
        assert_eq!(a.file.file_name().unwrap(), "f.txt");
        assert_eq!(b.file.file_name().unwrap(), "f.txt.2", "одинаковые имена не затирают друг друга");
        let info = fs::read_to_string(&a.info).unwrap();
        assert!(info.starts_with("[Trash Info]\nPath=/"), "{info}");
        assert!(info.contains("sub/f.txt\nDeletionDate=20"), "{info}");

        let listed = list(&[]);
        let la = listed.iter().find(|l| l.item == a).expect("в списке корзины");
        assert!(la.deleted > 0, "дата удаления разобрана");
        assert_eq!(la.item.original, base.join("data/sub/f.txt"), "путь раскодирован");

        restore(&b).unwrap();
        assert_eq!(fs::read(base.join("data/f.txt")).unwrap(), b"other");
        assert!(!list(&[]).iter().any(|l| l.item == b));
        assert!(!b.info.exists());
        // Место занято: не перезаписываем.
        fs::write(base.join("data/sub/f.txt"), b"new").unwrap();
        assert_eq!(restore(&a).unwrap_err().kind(), io::ErrorKind::AlreadyExists);

        fs::remove_dir_all(&base).unwrap();
    }
}
