//! Содержимое одной папки для основного вида: чтение, типы, сортировка.

use std::cmp::Ordering;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::mime;

#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    /// Папка (для ссылки — если ведёт на папку).
    pub is_dir: bool,
    pub is_symlink: bool,
    /// Ссылка, цель которой не существует.
    pub broken: bool,
    pub size: u64,
    /// Время изменения, секунды Unix.
    pub mtime: i64,
    pub mode: u32,
    pub mime: String,
    /// Для папки — сколько в ней элементов (`None`, если нет доступа).
    pub items: Option<u64>,
    /// Для папки — полный размер содержимого, если он уже посчитан анализом места.
    pub deep_size: Option<u64>,
    /// Только для содержимого корзины.
    pub trash: Option<TrashInfo>,
}

#[derive(Debug, Clone)]
pub struct TrashInfo {
    pub original: PathBuf,
    /// Время удаления, секунды Unix (0 — неизвестно).
    pub deleted: i64,
    pub info: PathBuf,
}

impl Entry {
    /// Скрытый, как в Thunar: начинается с точки или заканчивается тильдой (резервная копия).
    pub fn is_hidden(&self) -> bool {
        self.name.starts_with('.') || self.name.ends_with('~')
    }

    pub fn from_path(path: &Path) -> io::Result<Entry> {
        Self::read(path, true)
    }

    /// `count_items` — считать ли элементы внутри папок (это чтение каждой подпапки).
    pub fn read(path: &Path, count_items: bool) -> io::Result<Entry> {
        let lmeta = fs::symlink_metadata(path)?;
        let name = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
        let is_symlink = lmeta.file_type().is_symlink();
        let (meta, broken) = if is_symlink {
            match fs::metadata(path) {
                Ok(m) => (m, false),
                Err(_) => (lmeta, true),
            }
        } else {
            (lmeta, false)
        };
        let is_dir = meta.is_dir();
        // Тип только по имени: содержимое читать здесь слишком дорого (см. `mime::guess_fast`).
        let mime = if broken { mime::SYMLINK.to_string() } else { mime::db().guess_fast(&name, is_dir, meta.mode()) };
        let items = if is_dir && count_items { fs::read_dir(path).ok().map(|d| d.count() as u64) } else { None };
        Ok(Entry {
            name,
            path: path.to_path_buf(),
            is_dir,
            is_symlink,
            broken,
            size: if is_dir { 0 } else { meta.len() },
            mtime: meta.mtime(),
            mode: meta.mode(),
            mime,
            items,
            deep_size: None,
            trash: None,
        })
    }
}

/// Читает папку. Элементы, у которых не удалось прочитать свойства, пропускаются.
/// `count_items` — заполнять ли [`Entry::items`] у подпапок.
pub fn list(dir: &Path, count_items: bool) -> io::Result<Vec<Entry>> {
    let rd = fs::read_dir(dir)?;
    Ok(rd.flatten().filter_map(|e| Entry::read(&e.path(), count_items).ok()).collect())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Size,
    Type,
    Modified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sort {
    pub key: SortKey,
    pub ascending: bool,
    /// Папки всегда сверху (по умолчанию в Thunar включено).
    pub folders_first: bool,
}

impl Default for Sort {
    fn default() -> Self {
        Sort { key: SortKey::Name, ascending: true, folders_first: true }
    }
}

impl AsRef<Entry> for Entry {
    fn as_ref(&self) -> &Entry {
        self
    }
}

/// Сортирует всё, что содержит [`Entry`] (сами записи или строки интерфейса).
pub fn sort<T: AsRef<Entry>>(items: &mut [T], s: Sort) {
    let db = mime::db();
    items.sort_by(|a, b| {
        let (a, b) = (a.as_ref(), b.as_ref());
        if s.folders_first && a.is_dir != b.is_dir {
            return b.is_dir.cmp(&a.is_dir);
        }
        let ord = match s.key {
            SortKey::Name => Ordering::Equal,
            SortKey::Size => {
                if a.is_dir && b.is_dir {
                    // Настоящий размер, если известен, иначе число элементов.
                    match (a.deep_size, b.deep_size) {
                        (Some(x), Some(y)) => x.cmp(&y),
                        _ => a.items.cmp(&b.items),
                    }
                } else {
                    a.size.cmp(&b.size)
                }
            }
            SortKey::Type => db.describe(&a.mime).cmp(&db.describe(&b.mime)),
            SortKey::Modified => {
                let (x, y) = match (&a.trash, &b.trash) {
                    (Some(x), Some(y)) => (x.deleted, y.deleted),
                    _ => (a.mtime, b.mtime),
                };
                x.cmp(&y)
            }
        };
        // При равенстве — по имени, как в Thunar.
        let ord = ord.then_with(|| natural_cmp(&a.name, &b.name));
        if s.ascending { ord } else { ord.reverse() }
    });
}

/// Сравнение имён «по-человечески»: без учёта регистра, числа по значению
/// (`файл2` < `файл10`), ведущая точка скрытых файлов не влияет на порядок.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let strip = |s: &str| s.strip_prefix('.').unwrap_or(s).to_string();
    let (sa, sb) = (strip(a), strip(b));
    let mut x = sa.chars().peekable();
    let mut y = sb.chars().peekable();
    loop {
        match (x.peek().copied(), y.peek().copied()) {
            (None, None) => break,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(c), Some(d)) if c.is_ascii_digit() && d.is_ascii_digit() => {
                let take = |it: &mut std::iter::Peekable<std::str::Chars>| {
                    let mut n = String::new();
                    while let Some(c) = it.peek().copied().filter(char::is_ascii_digit) {
                        n.push(c);
                        it.next();
                    }
                    n
                };
                let (n, m) = (take(&mut x), take(&mut y));
                let (nt, mt) = (n.trim_start_matches('0'), m.trim_start_matches('0'));
                let ord = nt.len().cmp(&mt.len()).then_with(|| nt.cmp(mt)).then_with(|| n.len().cmp(&m.len()));
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            (Some(c), Some(d)) => {
                let ord = c.to_lowercase().cmp(d.to_lowercase());
                if ord != Ordering::Equal {
                    return ord;
                }
                x.next();
                y.next();
            }
        }
    }
    // Полностью равны без учёта регистра — стабильный порядок по исходной строке.
    a.cmp(b)
}

/// Дата для колонки «Изменён», как в Thunar: «Сегодня, 10:41», «Вчера, 18:03», «12.11.2026».
pub fn format_date(t: i64, now: i64) -> String {
    let tm = crate::trash::local_tm(t);
    let today = crate::trash::local_tm(now);
    let days = |tm: &libc::tm| (tm.tm_year as i64) * 400 + tm.tm_yday as i64;
    let time = format!("{:02}:{:02}", tm.tm_hour, tm.tm_min);
    match days(&today) - days(&tm) {
        0 => format!("Сегодня, {time}"),
        1 => format!("Вчера, {time}"),
        _ => format!("{:02}.{:02}.{:04}, {time}", tm.tm_mday, tm.tm_mon + 1, tm.tm_year + 1900),
    }
}

/// Имя пользователя по uid (`/etc/passwd` через libc), иначе число.
pub fn user_name(uid: u32) -> String {
    let mut buf = vec![0 as libc::c_char; 4096];
    let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
    let mut out: *mut libc::passwd = std::ptr::null_mut();
    let r = unsafe { libc::getpwuid_r(uid, &mut pw, buf.as_mut_ptr(), buf.len(), &mut out) };
    if r == 0 && !out.is_null() {
        return unsafe { std::ffi::CStr::from_ptr(pw.pw_name) }.to_string_lossy().into_owned();
    }
    uid.to_string()
}

/// Имя группы по gid, иначе число.
pub fn group_name(gid: u32) -> String {
    let mut buf = vec![0 as libc::c_char; 4096];
    let mut gr: libc::group = unsafe { std::mem::zeroed() };
    let mut out: *mut libc::group = std::ptr::null_mut();
    let r = unsafe { libc::getgrgid_r(gid, &mut gr, buf.as_mut_ptr(), buf.len(), &mut out) };
    if r == 0 && !out.is_null() {
        return unsafe { std::ffi::CStr::from_ptr(gr.gr_name) }.to_string_lossy().into_owned();
    }
    gid.to_string()
}

/// `rwxr-xr-x` из `st_mode`.
pub fn mode_string(mode: u32) -> String {
    let mut s = String::with_capacity(9);
    for shift in [6, 3, 0] {
        let m = (mode >> shift) & 7;
        s.push(if m & 4 != 0 { 'r' } else { '-' });
        s.push(if m & 2 != 0 { 'w' } else { '-' });
        s.push(if m & 1 != 0 { 'x' } else { '-' });
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_order() {
        let mut v = vec!["файл10", "Файл2", "файл1", ".bashrc", "Apple", "apple2", "b"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(v, ["Apple", "apple2", "b", ".bashrc", "файл1", "Файл2", "файл10"]);
        assert_eq!(natural_cmp("a01", "a1"), Ordering::Greater, "при равном числе короче — раньше");
        assert_eq!(natural_cmp("x", "x"), Ordering::Equal);
    }

    #[test]
    fn lists_and_sorts() {
        let d = std::env::temp_dir().join(format!("conchy-dir-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("Папка/внутри")).unwrap();
        fs::write(d.join("b.txt"), "1234567890").unwrap();
        fs::write(d.join("a10.txt"), "1").unwrap();
        fs::write(d.join("a9.txt"), "123").unwrap();
        fs::write(d.join(".hidden"), "").unwrap();
        std::os::unix::fs::symlink("nowhere", d.join("broken")).unwrap();

        assert_eq!(list(&d, false).unwrap().iter().find(|e| e.is_dir).unwrap().items, None);
        let mut v = list(&d, true).unwrap();
        sort(&mut v, Sort::default());
        let names: Vec<_> = v.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["Папка", "a9.txt", "a10.txt", "b.txt", "broken", ".hidden"]);
        assert_eq!(v[0].items, Some(1));
        assert!(v[0].is_dir && v[4].broken && v[4].is_symlink && v[5].is_hidden());

        sort(&mut v, Sort { key: SortKey::Size, ascending: false, folders_first: true });
        assert_eq!(v[0].name, "Папка");
        assert_eq!(v[1].name, "b.txt");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn dates_and_modes() {
        let now = 1_763_000_000; // ноябрь 2025
        assert!(format_date(now - 60, now).starts_with("Сегодня") || format_date(now - 60, now).starts_with("Вчера"));
        assert!(format_date(now - 86_400 * 30, now).contains('.'));
        assert_eq!(mode_string(0o100755), "rwxr-xr-x");
        assert_eq!(mode_string(0o640), "rw-r-----");
        assert_eq!(user_name(0), "root");
        assert_eq!(user_name(4_000_000_000), "4000000000");
    }
}
