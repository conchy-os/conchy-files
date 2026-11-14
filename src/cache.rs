//! Дисковый кеш анализа места (`~/.cache/conchy-files/trees`).
//!
//! Дерево сохраняется после долгого сканирования и при следующем запуске
//! показывается сразу, а в фоне перепроверяется. Формат свой и компактный:
//! обход в глубину, числа в varint, всё сжато LZ4.
//!
//! Ключ — UUID раздела и путь: разные флешки, смонтированные в одну точку,
//! не перепутаются. Файлы доступны только владельцу: в них имена всех файлов.
//!
//! Кеш не копится: [`DiskCache::cleanup`] удаляет записи пропавших папок, давно
//! не открывавшиеся и всё сверх лимита размера; при сохранении удаляются записи
//! вложенных папок, которые уже вошли в новое дерево.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::model::Tree;
use crate::scan::RawNode;

/// Версия 2: путь корня лежит в заголовке открыто, чтобы уборка не распаковывала файлы.
const MAGIC: &[u8; 8] = b"CONCHYT\x02";
/// Больше этого распакованный файл быть не может: защита от испорченного размера.
const MAX_RAW: usize = 1 << 30;

/// Сохранённый результат сканирования.
pub struct Cached {
    pub tree: Tree,
    /// Когда сканировали, секунды Unix.
    pub scanned_at: i64,
    pub took_ms: u64,
    pub errors: u64,
}

pub struct DiskCache {
    dir: PathBuf,
    max_bytes: u64,
    max_age: Duration,
}

impl DiskCache {
    pub fn new(dir: PathBuf, max_bytes: u64, max_age: Duration) -> Self {
        DiskCache { dir, max_bytes, max_age }
    }

    /// Удаляет весь кеш (когда его выключили в настройках).
    pub fn clear(dir: &Path) -> io::Result<()> {
        match fs::remove_dir_all(dir) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    /// `$XDG_CACHE_HOME/conchy-files/trees`, обычно `~/.cache/conchy-files/trees`.
    pub fn default_dir() -> PathBuf {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| crate::places::home().join(".cache"))
            .join("conchy-files/trees")
    }

    fn file_for(&self, root: &Path) -> PathBuf {
        self.dir.join(format!("{}.bin", key(root)))
    }

    /// Сохраняет дерево (атомарно: временный файл и переименование).
    pub fn save(&self, tree: &Tree, scanned_at: i64, took_ms: u64, errors: u64) -> io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        let _ = fs::set_permissions(&self.dir, fs::Permissions::from_mode(0o700));
        let data = encode(tree, scanned_at, took_ms, errors);
        let root = PathBuf::from(&*tree.node(Tree::ROOT).name);
        let path = self.file_for(&root);
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        {
            let mut f = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
            f.write_all(&data)?;
        }
        fs::rename(&tmp, &path)?;
        self.drop_nested(&root, &path);
        self.evict(&path);
        Ok(())
    }

    /// Уборка: записи пропавших папок, испорченные или старого формата,
    /// не открывавшиеся дольше `max_age`, забытые временные файлы, и всё сверх лимита.
    /// Возвращает, сколько файлов удалено.
    pub fn cleanup(&self) -> usize {
        let Ok(rd) = fs::read_dir(&self.dir) else { return 0 };
        let now = SystemTime::now();
        let mut removed = 0;
        for e in rd.flatten() {
            let p = e.path();
            let age = e.metadata().ok().and_then(|m| m.modified().ok()).and_then(|t| now.duration_since(t).ok());
            let stale = match p.extension().and_then(|x| x.to_str()) {
                Some("bin") => match read_root(&p) {
                    None => true,
                    Some(root) => !root.is_dir() || age.is_some_and(|a| a > self.max_age),
                },
                // Обрыв записи оставил временный файл.
                _ => age.is_some_and(|a| a > Duration::from_secs(3600)),
            };
            if stale && fs::remove_file(&p).is_ok() {
                removed += 1;
            }
        }
        self.evict(Path::new(""));
        removed
    }

    /// Записи папок внутри `root` на том же разделе уже вошли в его дерево — удаляем.
    /// Папки другого раздела (сканер туда не заходит) остаются.
    fn drop_nested(&self, root: &Path, keep: &Path) {
        use std::os::unix::fs::MetadataExt;
        let Ok(dev) = fs::metadata(root).map(|m| m.dev()) else { return };
        let Ok(rd) = fs::read_dir(&self.dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p == keep {
                continue;
            }
            let Some(other) = read_root(&p) else { continue };
            if other != root && other.starts_with(root) && fs::metadata(&other).is_ok_and(|m| m.dev() == dev) {
                let _ = fs::remove_file(&p);
            }
        }
    }

    /// Дерево, сохранённое ровно для `root`. Испорченный файл удаляется.
    pub fn load(&self, root: &Path) -> Option<Cached> {
        let path = self.file_for(root);
        let data = fs::read(&path).ok()?;
        match decode(&data) {
            Ok(c) if Path::new(&*c.tree.node(Tree::ROOT).name) == root => {
                // Отмечаем использование: при нехватке места уйдут давно не открывавшиеся.
                if let Ok(f) = fs::File::open(&path) {
                    let _ = f.set_modified(SystemTime::now());
                }
                Some(c)
            }
            // Совпал только хеш: это запись другой папки, её не трогаем.
            Ok(_) => None,
            Err(_) => {
                let _ = fs::remove_file(&path);
                None
            }
        }
    }

    /// Дерево для `path` или для ближайшей папки выше: из него берётся нужная подпапка.
    pub fn load_nearest(&self, path: &Path) -> Option<Cached> {
        path.ancestors().find_map(|p| {
            if !self.file_for(p).exists() {
                return None;
            }
            self.load(p)
        })
    }

    /// Держит кеш в пределах `max_bytes`, удаляя самые давние записи (кроме `keep`).
    fn evict(&self, keep: &Path) {
        let Ok(rd) = fs::read_dir(&self.dir) else { return };
        let mut files: Vec<(SystemTime, u64, PathBuf)> = rd
            .flatten()
            .filter_map(|e| {
                let m = e.metadata().ok()?;
                Some((m.modified().ok()?, m.len(), e.path()))
            })
            .collect();
        let mut total: u64 = files.iter().map(|f| f.1).sum();
        files.sort_by_key(|f| f.0);
        for (_, len, p) in files {
            if total <= self.max_bytes {
                break;
            }
            if p != keep {
                let _ = fs::remove_file(&p);
                total -= len;
            }
        }
    }
}

/// Ключ записи: раздел (по UUID, если есть) и путь. FNV-1a — стабилен между версиями Rust.
fn key(root: &Path) -> String {
    let volume = crate::places::mount_of(root).map(|(dev, _)| volume_id(&dev)).unwrap_or_default();
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in volume.as_bytes().iter().chain(b"\0").chain(root.as_os_str().as_encoded_bytes()) {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// UUID файловой системы устройства (`/dev/disk/by-uuid`), иначе само имя устройства.
fn volume_id(device: &str) -> String {
    let Ok(real) = fs::canonicalize(device) else { return device.to_string() };
    fs::read_dir("/dev/disk/by-uuid")
        .ok()
        .and_then(|rd| {
            rd.flatten()
                .find(|e| fs::canonicalize(e.path()).is_ok_and(|p| p == real))
                .map(|e| e.file_name().to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| device.to_string())
}

fn put_var(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    put_var(out, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}

/// Дерево → байты. Узлы в порядке обхода в глубину, у папки — число детей.
pub fn encode(tree: &Tree, scanned_at: i64, took_ms: u64, errors: u64) -> Vec<u8> {
    let mut raw = Vec::with_capacity(tree.nodes.len() * 24);
    put_var(&mut raw, scanned_at.max(0) as u64);
    put_var(&mut raw, took_ms);
    put_var(&mut raw, errors);
    let mut stack = vec![Tree::ROOT];
    while let Some(id) = stack.pop() {
        let n = tree.node(id);
        put_str(&mut raw, &n.name);
        raw.push(n.is_dir as u8);
        put_var(&mut raw, n.size);
        put_var(&mut raw, n.alloc);
        if n.is_dir {
            put_var(&mut raw, n.children.len() as u64);
            stack.extend(n.children.iter().rev());
        }
    }
    let mut out = MAGIC.to_vec();
    put_str(&mut out, &tree.node(Tree::ROOT).name);
    out.extend(lz4_flex::compress_prepend_size(&raw));
    out
}

/// Путь корня из заголовка файла, без распаковки. `None` — не наш файл или старый формат.
pub fn read_root(path: &Path) -> Option<PathBuf> {
    use std::io::Read;
    let mut head = vec![0u8; 8 + 10 + 4096];
    let n = fs::File::open(path).and_then(|mut f| f.read(&mut head)).ok()?;
    let mut r = Reader { b: head[..n].strip_prefix(MAGIC.as_slice())?, i: 0 };
    let root = r.str().ok()?;
    Some(PathBuf::from(&*root))
}

struct Reader<'a> {
    b: &'a [u8],
    i: usize,
}

impl Reader<'_> {
    fn var(&mut self) -> io::Result<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = *self.b.get(self.i).ok_or_else(bad)?;
            self.i += 1;
            v |= ((byte & 0x7f) as u64) << shift;
            if byte & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(bad())
    }

    fn str(&mut self) -> io::Result<Box<str>> {
        let len = self.var()? as usize;
        let s = self.b.get(self.i..self.i.checked_add(len).ok_or_else(bad)?).ok_or_else(bad)?;
        self.i += len;
        Ok(String::from_utf8_lossy(s).into())
    }

    fn node(&mut self) -> io::Result<(RawNode, u64)> {
        let name = self.str()?;
        let is_dir = *self.b.get(self.i).ok_or_else(bad)? != 0;
        self.i += 1;
        let size = self.var()?;
        let alloc = self.var()?;
        let kids = if is_dir { self.var()? } else { 0 };
        Ok((RawNode { name, is_dir, size, alloc, children: Vec::new() }, kids))
    }
}

fn bad() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "кеш анализа испорчен")
}

/// Байты → дерево. Любая порча данных — ошибка, а не паника.
pub fn decode(data: &[u8]) -> io::Result<Cached> {
    let after_magic = data.strip_prefix(MAGIC.as_slice()).ok_or_else(bad)?;
    let mut head = Reader { b: after_magic, i: 0 };
    head.str()?;
    let body = &after_magic[head.i..];
    let declared = body.get(..4).map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize).ok_or_else(bad)?;
    // LZ4 сжимает не сильнее ~255 раз: больший заявленный размер — порча, а не данные.
    if declared > MAX_RAW || declared > body.len().saturating_mul(256) {
        return Err(bad());
    }
    let raw = lz4_flex::decompress_size_prepended(body).map_err(|_| bad())?;
    let mut r = Reader { b: &raw, i: 0 };
    let scanned_at = r.var()? as i64;
    let took_ms = r.var()?;
    let errors = r.var()?;

    // Без рекурсии: стек незаконченных папок и сколько детей им ещё положено.
    let (root, root_kids) = r.node()?;
    let mut stack: Vec<(RawNode, u64)> = vec![(root, root_kids)];
    loop {
        let finished = stack.last().is_some_and(|(_, left)| *left == 0);
        if finished {
            let (done, _) = stack.pop().unwrap();
            match stack.last_mut() {
                Some((parent, left)) => {
                    parent.children.push(done);
                    *left -= 1;
                }
                None => {
                    if r.i != raw.len() {
                        return Err(bad());
                    }
                    return Ok(Cached { tree: Tree::from_raw(done), scanned_at, took_ms, errors });
                }
            }
            continue;
        }
        let (node, kids) = r.node()?;
        if node.is_dir && kids > 0 {
            stack.push((node, kids));
        } else {
            let (parent, left) = stack.last_mut().ok_or_else(bad)?;
            parent.children.push(node);
            *left -= 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{Progress, ScanOptions, scan};

    fn sample() -> Tree {
        let f = |n: &str, s: u64| RawNode { name: n.into(), size: s, alloc: s, ..Default::default() };
        Tree::from_raw(RawNode {
            name: "/data/Мои файлы".into(),
            is_dir: true,
            size: 1360,
            alloc: 1360,
            children: vec![
                f("a.mp4", 1000),
                RawNode { name: "пусто".into(), is_dir: true, ..Default::default() },
                RawNode {
                    name: "d".into(),
                    is_dir: true,
                    size: 360,
                    alloc: 360,
                    children: vec![f("b.txt", 300), f("c.txt", 60)],
                },
            ],
        })
    }

    fn paths(t: &Tree) -> Vec<(PathBuf, u64, bool)> {
        let mut v = Vec::new();
        let mut stack = vec![Tree::ROOT];
        while let Some(id) = stack.pop() {
            let n = t.node(id);
            v.push((t.path(id), n.size, n.is_dir));
            stack.extend(&n.children);
        }
        v.sort();
        v
    }

    #[test]
    fn roundtrip_keeps_structure() {
        let t = sample();
        let c = decode(&encode(&t, 1_763_000_000, 5900, 3)).unwrap();
        assert_eq!(paths(&c.tree), paths(&t));
        assert_eq!((c.scanned_at, c.took_ms, c.errors), (1_763_000_000, 5900, 3));
        assert_eq!(c.tree.node(Tree::ROOT).files, 3);
        assert_eq!(&*c.tree.types[0].ext, "mp4");
    }

    #[test]
    fn corrupt_data_is_an_error_not_a_panic() {
        let data = encode(&sample(), 1, 2, 3);
        assert!(decode(b"garbage").is_err());
        assert!(decode(&data[..data.len() - 3]).is_err());
        let mut flipped = data.clone();
        let mid = flipped.len() / 2;
        flipped[mid] ^= 0xff;
        let _ = decode(&flipped); // может оказаться и «валидным», главное — без паники
        // Заявленный размер распаковки сразу после заголовка с путём.
        let at = 8 + 1 + "/data/Мои файлы".len();
        let mut huge = data.clone();
        huge[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(decode(&huge).is_err());
        let mut v1 = data.clone();
        v1[7] = 1;
        assert!(decode(&v1).is_err(), "старый формат не читается");
    }

    #[test]
    fn save_load_nearest_and_evict() {
        let base = std::env::temp_dir().join(format!("conchy-cache-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let data = base.join("data");
        fs::create_dir_all(data.join("sub/deep")).unwrap();
        fs::write(data.join("sub/deep/f.bin"), vec![1u8; 5000]).unwrap();
        let tree = Tree::from_raw(scan(&data, &ScanOptions::default(), &Progress::default()).unwrap());

        let cache = DiskCache::new(base.join("cache"), 1 << 20, Duration::from_secs(3600));
        cache.save(&tree, 42, 1500, 0).unwrap();
        let file = cache.file_for(&data);
        assert_eq!(fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600, "только владельцу");

        let got = cache.load_nearest(&data.join("sub/deep")).expect("берётся дерево папки выше");
        assert_eq!(got.scanned_at, 42);
        assert!(got.tree.find(&data.join("sub/deep/f.bin")).is_some());
        assert!(cache.load(&data.join("sub")).is_none(), "для подпапки отдельной записи нет");

        assert_eq!(read_root(&file), Some(data.clone()), "путь читается без распаковки");

        // Испорченная запись удаляется и не мешает.
        fs::write(&file, b"CONCHYT\x02broken").unwrap();
        assert!(cache.load(&data).is_none());
        assert!(!file.exists());

        // Лимит: при переполнении уходит самая давняя запись.
        let tiny = DiskCache::new(base.join("tiny"), 1, Duration::from_secs(3600));
        let other = Tree::from_raw(RawNode { name: data.join("sub").display().to_string().into(), is_dir: true, ..Default::default() });
        tiny.save(&tree, 1, 1, 0).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        tiny.save(&other, 2, 1, 0).unwrap();
        assert!(tiny.load(&data).is_none(), "старую вытеснили");
        assert!(tiny.load(&data.join("sub")).is_some(), "только что записанную оставили");
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn cleanup_and_nested_entries() {
        let base = std::env::temp_dir().join(format!("conchy-cache-clean-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let data = base.join("data");
        fs::create_dir_all(data.join("sub")).unwrap();
        fs::create_dir_all(base.join("gone")).unwrap();
        let mk = |p: &Path| Tree::from_raw(scan(p, &ScanOptions::default(), &Progress::default()).unwrap());
        let dir = base.join("cache");
        let cache = DiskCache::new(dir.clone(), u64::MAX, Duration::from_secs(3600));

        // Вложенная запись исчезает, когда сохраняется дерево папки выше.
        cache.save(&mk(&data.join("sub")), 1, 1, 0).unwrap();
        cache.save(&mk(&base.join("gone")), 1, 1, 0).unwrap();
        assert!(cache.file_for(&data.join("sub")).exists());
        cache.save(&mk(&data), 1, 1, 0).unwrap();
        assert!(!cache.file_for(&data.join("sub")).exists(), "уже внутри дерева data");
        assert!(cache.file_for(&base.join("gone")).exists(), "не вложена — остаётся");

        // Уборка: папку удалили, мусор и старый формат — тоже прочь.
        fs::remove_dir_all(base.join("gone")).unwrap();
        fs::write(dir.join("junk.bin"), b"CONCHYT\x01old").unwrap();
        assert_eq!(cache.cleanup(), 2);
        assert!(cache.file_for(&data).exists(), "живая и свежая запись остаётся");

        // Срок жизни: запись, которую давно не открывали, удаляется.
        let old = DiskCache::new(dir.clone(), u64::MAX, Duration::from_secs(60));
        let f = fs::File::open(cache.file_for(&data)).unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(3600)).unwrap();
        assert_eq!(old.cleanup(), 1);

        DiskCache::clear(&dir).unwrap();
        assert!(!dir.exists());
        DiskCache::clear(&dir).unwrap();
        fs::remove_dir_all(&base).unwrap();
    }
}
