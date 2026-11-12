//! Операции с файлами: копирование, перемещение, переименование, создание,
//! корзина, удаление и их отмена.
//!
//! Модуль не знает об интерфейсе. Всё, что нужно спросить у пользователя
//! (конфликт имён), идёт через [`Asker`]; прогресс и отмена — через [`JobProgress`].
//! Поведение повторяет Thunar (`thunar-transfer-job.c`, `thunar-job-operation.c`):
//! - копия в ту же папку получает имя `файл (копия 1).txt`;
//! - при конфликте: заменить / пропустить / переименовать / объединить папки,
//!   с галочкой «для всех»;
//! - перемещение внутри раздела — мгновенный `rename`, между разделами — копия + удаление;
//! - каждая операция возвращает [`Record`], по которому её можно отменить.

use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::trash::{self, TrashedItem};

/// Счётчики, которые интерфейс читает, пока операция идёт в фоне.
#[derive(Debug, Default)]
pub struct JobProgress {
    pub total_bytes: AtomicU64,
    pub done_bytes: AtomicU64,
    pub total_items: AtomicU64,
    pub done_items: AtomicU64,
    /// Файл, над которым идёт работа.
    pub current: Mutex<PathBuf>,
    pub cancelled: AtomicBool,
}

impl JobProgress {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    /// Доля выполненного, 0..=1.
    pub fn fraction(&self) -> f32 {
        let tb = self.total_bytes.load(Ordering::Relaxed);
        let ti = self.total_items.load(Ordering::Relaxed);
        if tb > 0 {
            (self.done_bytes.load(Ordering::Relaxed) as f64 / tb as f64).min(1.0) as f32
        } else if ti > 0 {
            (self.done_items.load(Ordering::Relaxed) as f64 / ti as f64).min(1.0) as f32
        } else {
            0.0
        }
    }

    fn set_current(&self, p: &Path) {
        *self.current.lock().unwrap() = p.to_path_buf();
    }

    fn item_done(&self) {
        self.done_items.fetch_add(1, Ordering::Relaxed);
    }
}

/// Конфликт: на месте назначения уже что-то есть.
#[derive(Debug, Clone)]
pub struct Conflict {
    pub src: PathBuf,
    pub dst: PathBuf,
    pub src_is_dir: bool,
    pub dst_is_dir: bool,
}

impl Conflict {
    /// Можно ли предложить «Объединить» (папка в папку).
    pub fn can_merge(&self) -> bool {
        self.src_is_dir && self.dst_is_dir
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// Удалить то, что на месте назначения, и положить новое.
    Replace,
    /// Оставить как есть, этот элемент не трогать.
    Skip,
    /// Положить рядом под именем `имя (копия N)`.
    Rename,
    /// Папку в папку: переносить содержимое внутрь, конфликты решать для каждого файла.
    Merge,
    /// Остановить всю операцию.
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Answer {
    pub resolution: Resolution,
    /// Применять ко всем следующим конфликтам этого же вида (файл или папка).
    pub apply_to_all: bool,
}

/// Тот, кто решает конфликты. В интерфейсе это диалог, в тестах — заглушка.
/// Вызывается из фонового потока и может блокироваться, пока пользователь думает.
pub trait Asker: Sync {
    fn ask(&self, conflict: &Conflict) -> Answer;
}

/// Как отменить выполненную операцию.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Record {
    /// Созданные копии; отмена отправляет их в корзину.
    Copied(Vec<PathBuf>),
    /// Пары (откуда, куда); отмена переносит обратно.
    Moved(Vec<(PathBuf, PathBuf)>),
    Renamed { from: PathBuf, to: PathBuf },
    Trashed(Vec<TrashedItem>),
    /// Новая папка или пустой файл.
    Created(PathBuf),
}

impl Record {
    /// Подпись для пункта «Отменить …».
    pub fn describe(&self) -> String {
        fn count(n: usize) -> String {
            if n == 1 { String::new() } else { format!(" ({n})") }
        }
        match self {
            Record::Copied(v) => format!("копирование{}", count(v.len())),
            Record::Moved(v) => format!("перемещение{}", count(v.len())),
            Record::Renamed { .. } => "переименование".into(),
            Record::Trashed(v) => format!("удаление в корзину{}", count(v.len())),
            Record::Created(p) => format!("создание «{}»", display_name(p)),
        }
    }
}

/// Итог операции.
#[derive(Debug, Default)]
pub struct Outcome {
    /// По чему отменять. `None`, если ничего не изменилось.
    pub record: Option<Record>,
    /// Пути, которые изменились (появились, исчезли или поменялись внутри).
    /// Интерфейс перечитывает их с диска.
    pub touched: Vec<PathBuf>,
    pub errors: Vec<(PathBuf, String)>,
    pub cancelled: bool,
}

impl Outcome {
    fn error(&mut self, path: &Path, e: impl std::fmt::Display) {
        self.errors.push((path.to_path_buf(), e.to_string()));
    }

    fn touch(&mut self, path: &Path) {
        if !self.touched.iter().any(|p| p == path) {
            self.touched.push(path.to_path_buf());
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Copy,
    Move,
}

// ---------- копирование и перемещение ----------

/// Копирует или переносит `sources` внутрь папки `dest_dir`.
pub fn transfer(
    mode: Mode,
    sources: &[PathBuf],
    dest_dir: &Path,
    progress: &JobProgress,
    asker: &dyn Asker,
) -> Outcome {
    let mut t = Transfer::new(mode, progress, asker);

    // Сначала считаем объём, как Thunar («Подготовка…»), чтобы прогресс был честным.
    for src in sources {
        if progress.is_cancelled() {
            break;
        }
        let (bytes, items) = measure(src);
        progress.total_bytes.fetch_add(bytes, Ordering::Relaxed);
        progress.total_items.fetch_add(items, Ordering::Relaxed);
    }

    for src in sources {
        if progress.is_cancelled() {
            t.out.cancelled = true;
            break;
        }
        let Some(name) = src.file_name() else {
            t.out.error(src, "у пути нет имени");
            continue;
        };
        if dest_dir.starts_with(src) {
            t.out.error(src, "нельзя скопировать или переместить папку внутрь самой себя");
            continue;
        }
        let mut dst = dest_dir.join(name);
        if dst == *src {
            match mode {
                // Копия на то же место: Thunar молча даёт имя «(копия N)».
                Mode::Copy => dst = next_copy_name(dest_dir, name, src.is_dir()),
                // Перенос на то же место ничего не меняет.
                Mode::Move => {
                    let (b, i) = measure(src);
                    progress.done_bytes.fetch_add(b, Ordering::Relaxed);
                    progress.done_items.fetch_add(i, Ordering::Relaxed);
                    continue;
                }
            }
        }
        t.one(src, &dst);
        if mode == Mode::Move {
            t.out.touch(src);
        }
        t.out.touch(&dst);
    }
    if progress.is_cancelled() {
        t.out.cancelled = true;
    }

    let mut out = t.out;
    out.record = match mode {
        Mode::Copy if !t.created.is_empty() => Some(Record::Copied(t.created)),
        Mode::Move if !t.moved.is_empty() => Some(Record::Moved(t.moved)),
        _ => None,
    };
    out
}

struct Transfer<'a> {
    mode: Mode,
    progress: &'a JobProgress,
    asker: &'a dyn Asker,
    remembered_file: Option<Resolution>,
    remembered_dir: Option<Resolution>,
    out: Outcome,
    created: Vec<PathBuf>,
    moved: Vec<(PathBuf, PathBuf)>,
}

impl<'a> Transfer<'a> {
    fn new(mode: Mode, progress: &'a JobProgress, asker: &'a dyn Asker) -> Self {
        Transfer {
            mode,
            progress,
            asker,
            remembered_file: None,
            remembered_dir: None,
            out: Outcome::default(),
            created: Vec::new(),
            moved: Vec::new(),
        }
    }

    fn one(&mut self, src: &Path, dst: &Path) {
        if self.progress.is_cancelled() {
            return;
        }
        self.progress.set_current(src);
        let smeta = match fs::symlink_metadata(src) {
            Ok(m) => m,
            Err(e) => return self.out.error(src, e),
        };

        let mut dst = dst.to_path_buf();
        if let Ok(dmeta) = fs::symlink_metadata(&dst) {
            let conflict = Conflict {
                src: src.to_path_buf(),
                dst: dst.clone(),
                src_is_dir: smeta.is_dir(),
                dst_is_dir: dmeta.is_dir(),
            };
            match self.resolve(&conflict) {
                Resolution::Cancel => {
                    self.progress.cancel();
                    return;
                }
                Resolution::Skip => return self.skip(src),
                Resolution::Rename => {
                    let parent = dst.parent().unwrap_or(Path::new("/")).to_path_buf();
                    dst = next_copy_name(&parent, dst.file_name().unwrap(), smeta.is_dir());
                }
                Resolution::Replace => {
                    if src.starts_with(&dst) {
                        return self.out.error(src, "нельзя заменить папку, внутри которой лежит сам источник");
                    }
                    if let Err(e) = remove_all(&dst) {
                        return self.out.error(&dst, e);
                    }
                }
                Resolution::Merge => return self.merge(src, &dst),
            }
        }

        match self.mode {
            Mode::Copy => {
                if self.copy_tree(src, &dst, &smeta) {
                    self.created.push(dst);
                }
            }
            Mode::Move => match fs::rename(src, &dst) {
                Ok(()) => {
                    let (b, i) = measure(&dst);
                    self.progress.done_bytes.fetch_add(b, Ordering::Relaxed);
                    self.progress.done_items.fetch_add(i, Ordering::Relaxed);
                    self.moved.push((src.to_path_buf(), dst));
                }
                Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
                    // Другой раздел: копируем, и только если всё скопировалось, удаляем источник.
                    let errors_before = self.out.errors.len();
                    let copied = self.copy_tree(src, &dst, &smeta);
                    if copied && self.out.errors.len() == errors_before && !self.progress.is_cancelled() {
                        match remove_all(src) {
                            Ok(()) => self.moved.push((src.to_path_buf(), dst)),
                            Err(e) => self.out.error(src, format!("скопировано, но не удалось удалить источник: {e}")),
                        }
                    }
                }
                Err(e) => self.out.error(src, e),
            },
        }
    }

    fn resolve(&mut self, c: &Conflict) -> Resolution {
        let remembered = if c.can_merge() { self.remembered_dir } else { self.remembered_file };
        if let Some(r) = remembered {
            return r;
        }
        let mut a = self.asker.ask(c);
        if a.resolution == Resolution::Merge && !c.can_merge() {
            a.resolution = Resolution::Replace;
        }
        if a.apply_to_all && a.resolution != Resolution::Cancel {
            if c.can_merge() {
                self.remembered_dir = Some(a.resolution);
            } else {
                self.remembered_file = Some(a.resolution);
            }
        }
        a.resolution
    }

    fn skip(&mut self, src: &Path) {
        let (b, i) = measure(src);
        self.progress.done_bytes.fetch_add(b, Ordering::Relaxed);
        self.progress.done_items.fetch_add(i, Ordering::Relaxed);
    }

    /// Папка в существующую папку: содержимое по одному, конфликты — для каждого.
    fn merge(&mut self, src: &Path, dst: &Path) {
        self.progress.item_done();
        let entries = match fs::read_dir(src) {
            Ok(e) => e,
            Err(e) => return self.out.error(src, e),
        };
        for entry in entries {
            if self.progress.is_cancelled() {
                return;
            }
            match entry {
                Ok(entry) => self.one(&entry.path(), &dst.join(entry.file_name())),
                Err(e) => self.out.error(src, e),
            }
        }
        if self.mode == Mode::Move {
            // Пустая после переноса папка-источник больше не нужна; непустая
            // (что-то пропустили) остаётся на месте.
            let _ = fs::remove_dir(src);
        }
    }

    /// Рекурсивная копия. Ошибки внутри папки записываются, но не останавливают
    /// остальное. Возвращает `true`, если `dst` создан.
    fn copy_tree(&mut self, src: &Path, dst: &Path, meta: &fs::Metadata) -> bool {
        if self.progress.is_cancelled() {
            return false;
        }
        self.progress.set_current(src);
        let ft = meta.file_type();
        let res = if ft.is_symlink() {
            fs::read_link(src).and_then(|target| std::os::unix::fs::symlink(target, dst))
        } else if ft.is_dir() {
            if let Err(e) = fs::create_dir(dst) {
                self.out.error(dst, e);
                return false;
            }
            self.progress.item_done();
            match fs::read_dir(src) {
                Ok(entries) => {
                    for entry in entries {
                        if self.progress.is_cancelled() {
                            break;
                        }
                        let entry = match entry {
                            Ok(e) => e,
                            Err(e) => {
                                self.out.error(src, e);
                                continue;
                            }
                        };
                        let child = entry.path();
                        match fs::symlink_metadata(&child) {
                            Ok(m) => {
                                self.copy_tree(&child, &dst.join(entry.file_name()), &m);
                            }
                            Err(e) => self.out.error(&child, e),
                        }
                    }
                }
                Err(e) => self.out.error(src, e),
            }
            copy_attrs(dst, meta);
            return true;
        } else if ft.is_file() {
            self.copy_file(src, dst, meta)
        } else {
            Err(io::Error::new(io::ErrorKind::Unsupported, "особые файлы (сокеты, устройства, FIFO) не копируются"))
        };
        match res {
            Ok(()) => {
                self.progress.item_done();
                true
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted && self.progress.is_cancelled() => false,
            Err(e) => {
                self.out.error(src, e);
                false
            }
        }
    }

    fn copy_file(&self, src: &Path, dst: &Path, meta: &fs::Metadata) -> io::Result<()> {
        let mut from = File::open(src)?;
        let mut to = OpenOptions::new().write(true).create_new(true).open(dst)?;
        let mut buf = vec![0u8; 1 << 20];
        let result = (|| {
            loop {
                if self.progress.is_cancelled() {
                    return Err(io::Error::new(io::ErrorKind::Interrupted, "отменено"));
                }
                let n = from.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                to.write_all(&buf[..n])?;
                self.progress.done_bytes.fetch_add(n as u64, Ordering::Relaxed);
            }
            Ok(())
        })();
        if let Err(e) = result {
            // Недокопированный файл хуже отсутствующего.
            drop(to);
            let _ = fs::remove_file(dst);
            return Err(e);
        }
        drop(to);
        copy_attrs(dst, meta);
        Ok(())
    }
}

/// Права и время изменения, как у `cp -p` (владельца не трогаем).
fn copy_attrs(dst: &Path, meta: &fs::Metadata) {
    let _ = fs::set_permissions(dst, meta.permissions());
    if let Ok(mtime) = meta.modified() {
        if let Ok(f) = File::open(dst) {
            let _ = f.set_modified(mtime);
        }
    }
}

/// Объём и число элементов для прогресса (без перехода по симлинкам).
fn measure(path: &Path) -> (u64, u64) {
    let Ok(meta) = fs::symlink_metadata(path) else { return (0, 0) };
    if !meta.is_dir() {
        return (if meta.is_file() { meta.len() } else { 0 }, 1);
    }
    let mut bytes = 0;
    let mut items = 1;
    if let Ok(entries) = fs::read_dir(path) {
        for e in entries.flatten() {
            let (b, i) = measure(&e.path());
            bytes += b;
            items += i;
        }
    }
    (bytes, items)
}

fn remove_all(path: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.is_dir() { fs::remove_dir_all(path) } else { fs::remove_file(path) }
}

// ---------- простые операции ----------

/// Проверка имени файла, которое ввёл пользователь.
pub fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Имя не может быть пустым".into());
    }
    if name == "." || name == ".." {
        return Err("Имена «.» и «..» зарезервированы".into());
    }
    if name.contains('/') {
        return Err("Имя не может содержать «/»".into());
    }
    if name.contains('\0') {
        return Err("Имя не может содержать нулевой байт".into());
    }
    if name.len() > 255 {
        return Err("Слишком длинное имя (больше 255 байт)".into());
    }
    Ok(())
}

pub fn rename(path: &Path, new_name: &str) -> Outcome {
    let mut out = Outcome::default();
    if let Err(e) = validate_name(new_name) {
        out.error(path, e);
        return out;
    }
    let Some(parent) = path.parent() else {
        out.error(path, "корень нельзя переименовать");
        return out;
    };
    let to = parent.join(new_name);
    if to == path {
        return out;
    }
    // Разрешаем менять только регистр букв на том же файле, остальное — конфликт.
    if fs::symlink_metadata(&to).is_ok() && !same_file(path, &to) {
        out.error(path, format!("«{new_name}» уже существует"));
        return out;
    }
    match fs::rename(path, &to) {
        Ok(()) => {
            out.touch(path);
            out.touch(&to);
            out.record = Some(Record::Renamed { from: path.to_path_buf(), to });
        }
        Err(e) => out.error(path, e),
    }
    out
}

fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (fs::symlink_metadata(a), fs::symlink_metadata(b)) {
        (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
        _ => false,
    }
}

/// Новая папка или пустой файл в `parent`.
pub fn create(parent: &Path, name: &str, is_dir: bool) -> Outcome {
    let mut out = Outcome::default();
    if let Err(e) = validate_name(name) {
        out.error(parent, e);
        return out;
    }
    let path = parent.join(name);
    let res = if is_dir {
        fs::create_dir(&path)
    } else {
        OpenOptions::new().write(true).create_new(true).open(&path).map(drop)
    };
    match res {
        Ok(()) => {
            out.touch(&path);
            out.record = Some(Record::Created(path));
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => out.error(&path, format!("«{name}» уже существует")),
        Err(e) => out.error(&path, e),
    }
    out
}

pub fn trash_all(paths: &[PathBuf], progress: &JobProgress) -> Outcome {
    let mut out = Outcome::default();
    let mut items = Vec::new();
    progress.total_items.store(paths.len() as u64, Ordering::Relaxed);
    for p in paths {
        if progress.is_cancelled() {
            out.cancelled = true;
            break;
        }
        progress.set_current(p);
        match trash::trash(p) {
            Ok(item) => {
                out.touch(p);
                items.push(item);
            }
            Err(e) => out.error(p, e),
        }
        progress.item_done();
    }
    if !items.is_empty() {
        out.record = Some(Record::Trashed(items));
    }
    out
}

/// Удаление насовсем, без корзины. Отменить нельзя.
pub fn delete_all(paths: &[PathBuf], progress: &JobProgress) -> Outcome {
    let mut out = Outcome::default();
    progress.total_items.store(paths.len() as u64, Ordering::Relaxed);
    for p in paths {
        if progress.is_cancelled() {
            out.cancelled = true;
            break;
        }
        progress.set_current(p);
        match remove_all(p) {
            Ok(()) => out.touch(p),
            Err(e) => out.error(p, e),
        }
        progress.item_done();
    }
    out
}

/// Возвращает файлы из корзины на прежние места (пункт «Восстановить»).
/// Отменяется как обычное удаление в корзину не будет: запись не создаётся.
pub fn restore_all(items: &[TrashedItem], progress: &JobProgress) -> Outcome {
    let mut out = Outcome::default();
    progress.total_items.store(items.len() as u64, Ordering::Relaxed);
    for item in items {
        if progress.is_cancelled() {
            out.cancelled = true;
            break;
        }
        progress.set_current(&item.original);
        match trash::restore(item) {
            Ok(()) => {
                out.touch(&item.original);
                out.touch(&item.file);
            }
            Err(e) => out.error(&item.original, e),
        }
        progress.item_done();
    }
    out
}

/// Удаляет из корзины насовсем (в том числе «Очистить корзину»).
pub fn erase_all(items: &[TrashedItem], progress: &JobProgress) -> Outcome {
    let mut out = Outcome::default();
    progress.total_items.store(items.len() as u64, Ordering::Relaxed);
    for item in items {
        if progress.is_cancelled() {
            out.cancelled = true;
            break;
        }
        progress.set_current(&item.original);
        match trash::erase(item) {
            Ok(()) => out.touch(&item.file),
            Err(e) => out.error(&item.original, e),
        }
        progress.item_done();
    }
    out
}

// ---------- отмена ----------

/// Выполняет обратную операцию.
pub fn undo(record: &Record, progress: &JobProgress) -> Outcome {
    let mut out = Outcome::default();
    match record {
        // Копии не удаляем насовсем, а отправляем в корзину: вдруг в них уже что-то поменяли.
        Record::Copied(created) => {
            let mut o = trash_all(created, progress);
            o.record = None;
            return o;
        }
        Record::Moved(pairs) => {
            progress.total_items.store(pairs.len() as u64, Ordering::Relaxed);
            for (from, to) in pairs.iter().rev() {
                progress.set_current(to);
                if fs::symlink_metadata(from).is_ok() {
                    out.error(from, "на прежнем месте уже что-то есть");
                    continue;
                }
                if let Some(parent) = from.parent() {
                    if let Err(e) = fs::create_dir_all(parent) {
                        out.error(from, e);
                        continue;
                    }
                }
                // Тот же перенос, что и вперёд: rename, а между разделами — копия + удаление.
                let mut t = Transfer::new(Mode::Move, progress, &NeverAsk);
                t.one(to, from);
                let res = match t.out.errors.into_iter().next() {
                    Some((_, e)) => Err(io::Error::other(e)),
                    None => Ok(()),
                };
                match res {
                    Ok(()) => {
                        out.touch(to);
                        out.touch(from);
                    }
                    Err(e) => out.error(to, e),
                }
                progress.item_done();
            }
        }
        Record::Renamed { from, to } => {
            if fs::symlink_metadata(from).is_ok() && !same_file(from, to) {
                out.error(from, "прежнее имя уже занято");
            } else {
                match fs::rename(to, from) {
                    Ok(()) => {
                        out.touch(to);
                        out.touch(from);
                    }
                    Err(e) => out.error(to, e),
                }
            }
        }
        Record::Trashed(items) => {
            progress.total_items.store(items.len() as u64, Ordering::Relaxed);
            for item in items {
                progress.set_current(&item.original);
                match trash::restore(item) {
                    Ok(()) => out.touch(&item.original),
                    Err(e) => out.error(&item.original, e),
                }
                progress.item_done();
            }
        }
        Record::Created(path) => {
            // Пустое удаляем сразу, а если в созданное уже что-то положили — в корзину.
            let empty = match fs::symlink_metadata(path) {
                Ok(m) if m.is_dir() => fs::read_dir(path).map(|mut d| d.next().is_none()).unwrap_or(false),
                Ok(m) => m.len() == 0,
                Err(e) => {
                    out.error(path, e);
                    return out;
                }
            };
            let res = if empty { remove_all(path) } else { trash::trash(path).map(drop) };
            match res {
                Ok(()) => out.touch(path),
                Err(e) => out.error(path, e),
            }
        }
    }
    out
}

/// Для внутренних переносов, где конфликтов быть не должно.
struct NeverAsk;
impl Asker for NeverAsk {
    fn ask(&self, _: &Conflict) -> Answer {
        Answer { resolution: Resolution::Skip, apply_to_all: true }
    }
}

// ---------- имена ----------

/// Делит имя на основу и расширение так же, как Thunar: `a.tar.gz` → (`a`, `.tar.gz`),
/// `.bashrc` → (`.bashrc`, ``). У папок расширения нет.
pub fn split_ext(name: &[u8], is_dir: bool) -> (&[u8], &[u8]) {
    const COMPRESSED: [&[u8]; 8] = [b"gz", b"bz2", b"lzma", b"lrz", b"rpm", b"lzo", b"xz", b"z"];
    if is_dir {
        return (name, b"");
    }
    let Some(dot) = name.iter().rposition(|&b| b == b'.') else { return (name, b"") };
    if dot == 0 || dot + 1 == name.len() {
        return (name, b"");
    }
    let ext = &name[dot + 1..];
    let mut cut = dot;
    if COMPRESSED.iter().any(|c| c.eq_ignore_ascii_case(ext)) {
        // Контейнер перед сжатием: `.tar.gz`, `.psd.bz2` (2–5 символов).
        if let Some(dot2) = name[..dot].iter().rposition(|&b| b == b'.') {
            let len = dot - dot2 - 1;
            if dot2 != 0 && (2..=5).contains(&len) {
                cut = dot2;
            }
        }
    }
    (&name[..cut], &name[cut..])
}

/// Первое свободное имя вида `имя (копия N).ext` в папке `dir`.
pub fn next_copy_name(dir: &Path, name: &OsStr, is_dir: bool) -> PathBuf {
    let (base, ext) = split_ext(name.as_bytes(), is_dir);
    for n in 1u32.. {
        let mut v = base.to_vec();
        v.extend_from_slice(format!(" (копия {n})").as_bytes());
        v.extend_from_slice(ext);
        let p = dir.join(OsString::from_vec(v));
        if fs::symlink_metadata(&p).is_err() {
            return p;
        }
    }
    unreachable!()
}

/// Первое свободное имя для нового элемента: `Новая папка`, `Новая папка 1`, …
pub fn next_new_name(dir: &Path, name: &str, is_dir: bool) -> String {
    if fs::symlink_metadata(dir.join(name)).is_err() {
        return name.to_string();
    }
    let (base, ext) = split_ext(name.as_bytes(), is_dir);
    let (base, ext) = (String::from_utf8_lossy(base), String::from_utf8_lossy(ext));
    for n in 1u32.. {
        let cand = format!("{base} {n}{ext}");
        if fs::symlink_metadata(dir.join(&cand)).is_err() {
            return cand;
        }
    }
    unreachable!()
}

/// Дата изменения в местном времени: `12.11.2026 10:41`.
pub fn format_mtime(meta: &fs::Metadata) -> String {
    use std::os::unix::fs::MetadataExt;
    let tm = trash::local_tm(meta.mtime());
    format!(
        "{:02}.{:02}.{:04} {:02}:{:02}",
        tm.tm_mday,
        tm.tm_mon + 1,
        tm.tm_year + 1900,
        tm.tm_hour,
        tm.tm_min
    )
}

pub fn display_name(p: &Path) -> String {
    p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    struct Fixed(Resolution, bool, AtomicUsize);
    impl Asker for Fixed {
        fn ask(&self, _: &Conflict) -> Answer {
            self.2.fetch_add(1, Ordering::Relaxed);
            Answer { resolution: self.0, apply_to_all: self.1 }
        }
    }
    fn asker(r: Resolution, all: bool) -> Fixed {
        Fixed(r, all, AtomicUsize::new(0))
    }

    fn tmp(tag: &str) -> PathBuf {
        crate::test_xdg();
        let d = std::env::temp_dir().join(format!("conchy-ops-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn read(p: &Path) -> String {
        fs::read_to_string(p).unwrap()
    }

    #[test]
    fn names_like_thunar() {
        let s = |n: &str, d| {
            let (a, b) = split_ext(n.as_bytes(), d);
            (String::from_utf8(a.to_vec()).unwrap(), String::from_utf8(b.to_vec()).unwrap())
        };
        assert_eq!(s("photo.jpg", false), ("photo".into(), ".jpg".into()));
        assert_eq!(s("a.tar.gz", false), ("a".into(), ".tar.gz".into()));
        assert_eq!(s("a.b.gz", false), ("a.b".into(), ".gz".into()), "контейнер короче 2 символов");
        assert_eq!(s(".bashrc", false), (".bashrc".into(), "".into()));
        assert_eq!(s("dir.d", true), ("dir.d".into(), "".into()));
        assert_eq!(s("trailing.", false), ("trailing.".into(), "".into()));

        let d = tmp("names");
        fs::write(d.join("a.txt"), "").unwrap();
        fs::write(d.join("a (копия 1).txt"), "").unwrap();
        assert_eq!(next_copy_name(&d, OsStr::new("a.txt"), false), d.join("a (копия 2).txt"));
        fs::create_dir(d.join("Новая папка")).unwrap();
        assert_eq!(next_new_name(&d, "Новая папка", true), "Новая папка 1");
        assert_eq!(next_new_name(&d, "Новый файл.txt", false), "Новый файл.txt");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn copy_tree_and_duplicate_in_place() {
        let d = tmp("copy");
        fs::create_dir_all(d.join("src/inner")).unwrap();
        fs::write(d.join("src/inner/f.txt"), "data").unwrap();
        std::os::unix::fs::symlink("inner/f.txt", d.join("src/link")).unwrap();
        fs::create_dir(d.join("dst")).unwrap();

        let p = JobProgress::default();
        let a = asker(Resolution::Cancel, false);
        let o = transfer(Mode::Copy, &[d.join("src")], &d.join("dst"), &p, &a);
        assert!(o.errors.is_empty(), "{:?}", o.errors);
        assert_eq!(read(&d.join("dst/src/inner/f.txt")), "data");
        assert_eq!(fs::read_link(d.join("dst/src/link")).unwrap(), Path::new("inner/f.txt"), "симлинк копируется как симлинк");
        assert_eq!(o.record, Some(Record::Copied(vec![d.join("dst/src")])));
        assert_eq!(p.fraction(), 1.0);

        // Копия в ту же папку — без вопросов, с новым именем.
        let o = transfer(Mode::Copy, &[d.join("src/inner/f.txt")], &d.join("src/inner"), &JobProgress::default(), &a);
        assert_eq!(a.2.load(Ordering::Relaxed), 0);
        assert_eq!(o.record, Some(Record::Copied(vec![d.join("src/inner/f (копия 1).txt")])));

        // Папку в саму себя нельзя.
        let o = transfer(Mode::Copy, &[d.join("src")], &d.join("src/inner"), &JobProgress::default(), &a);
        assert_eq!(o.errors.len(), 1);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn conflicts_skip_replace_rename_merge() {
        let d = tmp("conflict");
        fs::create_dir_all(d.join("a/sub")).unwrap();
        fs::write(d.join("a/x.txt"), "new-x").unwrap();
        fs::write(d.join("a/sub/y.txt"), "new-y").unwrap();
        fs::create_dir_all(d.join("b/a/sub")).unwrap();
        fs::write(d.join("b/a/x.txt"), "old-x").unwrap();
        fs::write(d.join("b/a/keep.txt"), "keep").unwrap();

        // Папка в папку + «для всех»: объединяем, файлы заменяем.
        struct Merge;
        impl Asker for Merge {
            fn ask(&self, c: &Conflict) -> Answer {
                let resolution = if c.can_merge() { Resolution::Merge } else { Resolution::Replace };
                Answer { resolution, apply_to_all: true }
            }
        }
        let o = transfer(Mode::Copy, &[d.join("a")], &d.join("b"), &JobProgress::default(), &Merge);
        assert!(o.errors.is_empty(), "{:?}", o.errors);
        assert_eq!(read(&d.join("b/a/x.txt")), "new-x");
        assert_eq!(read(&d.join("b/a/keep.txt")), "keep", "объединение не трогает лишнее");
        assert_eq!(read(&d.join("b/a/sub/y.txt")), "new-y");
        // В отмену попадают только реально созданные файлы, а не существовавшие папки.
        let Some(Record::Copied(mut v)) = o.record else { panic!() };
        v.sort();
        assert_eq!(v, vec![d.join("b/a/sub/y.txt"), d.join("b/a/x.txt")]);

        // Пропустить.
        fs::write(d.join("a/x.txt"), "newer").unwrap();
        let skip = asker(Resolution::Skip, false);
        transfer(Mode::Copy, &[d.join("a/x.txt")], &d.join("b/a"), &JobProgress::default(), &skip);
        assert_eq!(read(&d.join("b/a/x.txt")), "new-x");

        // Переименовать.
        let ren = asker(Resolution::Rename, false);
        let o = transfer(Mode::Copy, &[d.join("a/x.txt")], &d.join("b/a"), &JobProgress::default(), &ren);
        assert_eq!(o.record, Some(Record::Copied(vec![d.join("b/a/x (копия 1).txt")])));
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn move_and_undo() {
        let d = tmp("move");
        fs::create_dir_all(d.join("from/dir")).unwrap();
        fs::write(d.join("from/dir/f"), "1").unwrap();
        fs::write(d.join("from/g"), "2").unwrap();
        fs::create_dir(d.join("to")).unwrap();

        let a = asker(Resolution::Cancel, false);
        let srcs = [d.join("from/dir"), d.join("from/g")];
        let o = transfer(Mode::Move, &srcs, &d.join("to"), &JobProgress::default(), &a);
        assert!(o.errors.is_empty());
        assert!(!d.join("from/g").exists());
        assert_eq!(read(&d.join("to/dir/f")), "1");
        assert!(o.touched.contains(&d.join("from/g")) && o.touched.contains(&d.join("to/g")));

        let u = undo(o.record.as_ref().unwrap(), &JobProgress::default());
        assert!(u.errors.is_empty(), "{:?}", u.errors);
        assert_eq!(read(&d.join("from/dir/f")), "1");
        assert_eq!(read(&d.join("from/g")), "2");
        assert!(!d.join("to/g").exists());

        // Перенос в ту же папку — ничего не происходит.
        let o = transfer(Mode::Move, &[d.join("from/g")], &d.join("from"), &JobProgress::default(), &a);
        assert!(o.record.is_none() && o.errors.is_empty());
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn rename_create_trash_and_undo() {
        let d = tmp("simple");
        fs::write(d.join("a.txt"), "x").unwrap();
        fs::write(d.join("taken"), "").unwrap();

        assert_eq!(rename(&d.join("a.txt"), "taken").errors.len(), 1, "чужое имя не затираем");
        assert_eq!(rename(&d.join("a.txt"), "a/b").errors.len(), 1);
        let o = rename(&d.join("a.txt"), "b.txt");
        assert!(d.join("b.txt").exists());
        undo(o.record.as_ref().unwrap(), &JobProgress::default());
        assert!(d.join("a.txt").exists() && !d.join("b.txt").exists());

        let o = create(&d, "Новая папка", true);
        assert!(d.join("Новая папка").is_dir());
        assert_eq!(create(&d, "Новая папка", true).errors.len(), 1);
        undo(o.record.as_ref().unwrap(), &JobProgress::default());
        assert!(!d.join("Новая папка").exists());

        let o = trash_all(&[d.join("a.txt")], &JobProgress::default());
        assert!(o.errors.is_empty(), "{:?}", o.errors);
        assert!(!d.join("a.txt").exists());
        undo(o.record.as_ref().unwrap(), &JobProgress::default());
        assert_eq!(read(&d.join("a.txt")), "x");

        // Отмена копирования убирает копию в корзину, оригинал остаётся.
        let o = transfer(Mode::Copy, &[d.join("a.txt")], &d, &JobProgress::default(), &asker(Resolution::Cancel, false));
        assert!(d.join("a (копия 1).txt").exists());
        undo(o.record.as_ref().unwrap(), &JobProgress::default());
        assert!(!d.join("a (копия 1).txt").exists() && d.join("a.txt").exists());

        let o = delete_all(&[d.join("taken")], &JobProgress::default());
        assert!(o.record.is_none() && !d.join("taken").exists());
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn cancel_stops_and_cleans_partial_file() {
        let d = tmp("cancel");
        fs::write(d.join("big"), vec![1u8; 3 << 20]).unwrap();
        fs::create_dir(d.join("out")).unwrap();
        let p = JobProgress::default();
        p.cancel();
        let o = transfer(Mode::Copy, &[d.join("big")], &d.join("out"), &p, &asker(Resolution::Cancel, false));
        assert!(o.cancelled);
        assert!(!d.join("out/big").exists());
        fs::remove_dir_all(&d).unwrap();
    }
}
