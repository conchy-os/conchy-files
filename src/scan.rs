//! Параллельный обход файловой системы.
//!
//! Обход идёт по всем ядрам (rayon), не переходит на другие файловые системы
//! и не следует по символическим ссылкам. Жёсткие ссылки учитываются один раз.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use rayon::prelude::*;

/// Узел, собранный сканером. Потом превращается в плоское дерево [`crate::model::Tree`].
#[derive(Debug, Default)]
pub struct RawNode {
    pub name: Box<str>,
    pub is_dir: bool,
    /// Видимый размер (как у `ls -l`).
    pub size: u64,
    /// Реально занятое место (`st_blocks * 512`): учитывает сжатие и разреженные файлы.
    pub alloc: u64,
    pub children: Vec<RawNode>,
}

/// Счётчики прогресса, которые интерфейс читает во время сканирования.
#[derive(Debug, Default)]
pub struct Progress {
    pub files: AtomicU64,
    pub dirs: AtomicU64,
    pub bytes: AtomicU64,
    pub errors: AtomicU64,
    pub cancelled: AtomicBool,
}

#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// Не заходить в другие файловые системы (`/proc`, флешки, сетевые диски).
    pub one_file_system: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self { one_file_system: true }
    }
}

struct Ctx<'a> {
    root_dev: u64,
    opts: &'a ScanOptions,
    progress: &'a Progress,
    /// (dev, ino) файлов с несколькими жёсткими ссылками, которые уже посчитаны.
    seen_links: Mutex<HashSet<(u64, u64)>>,
}

/// Сканирует `root` и возвращает дерево. Ошибки доступа к отдельным папкам
/// не прерывают обход, а считаются в `progress.errors`.
pub fn scan(root: &Path, opts: &ScanOptions, progress: &Progress) -> anyhow::Result<RawNode> {
    let meta = fs::symlink_metadata(root)?;
    let ctx = Ctx {
        root_dev: meta.dev(),
        opts,
        progress,
        seen_links: Mutex::new(HashSet::new()),
    };
    let name = root.to_string_lossy().into();
    if !meta.is_dir() {
        return Ok(file_node(name, &meta, &ctx));
    }
    progress.dirs.fetch_add(1, Ordering::Relaxed);
    Ok(scan_dir(root.to_path_buf(), name, &meta, &ctx))
}

fn scan_dir(path: PathBuf, name: Box<str>, meta: &fs::Metadata, ctx: &Ctx) -> RawNode {
    let mut node = RawNode {
        name,
        is_dir: true,
        size: 0,
        alloc: meta.blocks() * 512,
        children: Vec::new(),
    };
    if ctx.progress.cancelled.load(Ordering::Relaxed) {
        return node;
    }

    let entries = match fs::read_dir(&path) {
        Ok(e) => e,
        Err(_) => {
            ctx.progress.errors.fetch_add(1, Ordering::Relaxed);
            return node;
        }
    };

    let mut subdirs = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else {
            ctx.progress.errors.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        // DirEntry::metadata не следует по символическим ссылкам.
        let Ok(meta) = entry.metadata() else {
            ctx.progress.errors.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        let child_name = os_to_box(&entry.file_name());
        if meta.is_dir() {
            if ctx.opts.one_file_system && meta.dev() != ctx.root_dev {
                // Точка монтирования другой ФС: показываем пустой папкой.
                node.children.push(RawNode { name: child_name, is_dir: true, ..Default::default() });
                continue;
            }
            subdirs.push((entry.path(), child_name, meta));
        } else {
            node.children.push(file_node(child_name, &meta, ctx));
        }
    }

    let dirs: Vec<RawNode> = subdirs
        .into_par_iter()
        .map(|(p, n, m)| {
            ctx.progress.dirs.fetch_add(1, Ordering::Relaxed);
            scan_dir(p, n, &m, ctx)
        })
        .collect();
    node.children.extend(dirs);

    for c in &node.children {
        node.size += c.size;
        node.alloc += c.alloc;
    }
    node
}

fn file_node(name: Box<str>, meta: &fs::Metadata, ctx: &Ctx) -> RawNode {
    ctx.progress.files.fetch_add(1, Ordering::Relaxed);
    let duplicate = meta.nlink() > 1
        && !ctx.seen_links.lock().unwrap().insert((meta.dev(), meta.ino()));
    let (size, alloc) = if duplicate { (0, 0) } else { (meta.len(), meta.blocks() * 512) };
    ctx.progress.bytes.fetch_add(size, Ordering::Relaxed);
    RawNode { name, is_dir: false, size, alloc, children: Vec::new() }
}

fn os_to_box(s: &OsStr) -> Box<str> {
    s.to_string_lossy().into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("conchy-files-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(path: &Path, bytes: usize) {
        let mut f = fs::File::create(path).unwrap();
        f.write_all(&vec![7u8; bytes]).unwrap();
    }

    #[test]
    fn sums_sizes_and_dedups_hardlinks() {
        let d = tmpdir("sum");
        fs::create_dir(d.join("a")).unwrap();
        write(&d.join("a/one.bin"), 1000);
        write(&d.join("two.txt"), 500);
        fs::hard_link(d.join("two.txt"), d.join("a/two-link.txt")).unwrap();
        std::os::unix::fs::symlink(d.join("a"), d.join("loop")).unwrap();

        let p = Progress::default();
        let root = scan(&d, &ScanOptions::default(), &p).unwrap();
        // Размер симлинка — длина пути, на который он указывает.
        let link_len = fs::symlink_metadata(d.join("loop")).unwrap().len();
        assert_eq!(root.size, 1500 + link_len, "жёсткая ссылка считается один раз");
        assert_eq!(p.files.load(Ordering::Relaxed), 4); // 2 файла + ссылка + симлинк
        let a = root.children.iter().find(|c| &*c.name == "a").unwrap();
        assert!(a.is_dir);
        let lnk = root.children.iter().find(|c| &*c.name == "loop").unwrap();
        assert!(!lnk.is_dir, "по симлинку не идём");
        fs::remove_dir_all(&d).unwrap();
    }
}
