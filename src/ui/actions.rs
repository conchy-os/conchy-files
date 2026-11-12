//! Команды над файлами (буфер обмена, переименование, корзина, отмена)
//! и их выполнение в фоне с обновлением дерева.
//!
//! Одновременно идёт одна операция. Конфликты имён из фонового потока приходят
//! через канал и показываются диалогом; поток ждёт ответа.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use conchy_files::model::{NodeId, Tree, human_size};
use conchy_files::ops::{self, Answer, Asker, Conflict, JobProgress, Mode, Outcome, Record, Resolution};
use conchy_files::scan::{Progress, RawNode, ScanOptions, scan};
use gpui::{ClipboardItem, Context, Task, Timer};

use super::FilesView;
use super::dialogs::{Dialog, NamePurpose};
use super::field::TextField;

const UNDO_LIMIT: usize = 100;
const NOTICE_TTL: Duration = Duration::from_secs(4);

/// Команда из меню или с клавиатуры.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Cmd {
    Open,
    Cut,
    Copy,
    Paste,
    Rename,
    NewFolder,
    NewFile,
    Trash,
    Delete,
    Undo,
    CopyPath,
}

/// Что лежит в буфере обмена приложения.
#[derive(Debug, Clone)]
pub(super) struct Clip {
    pub mode: Mode,
    pub paths: Vec<PathBuf>,
}

/// Вопрос о конфликте из фонового потока.
pub(super) struct ConflictRequest {
    pub conflict: Conflict,
    /// «Размер · дата» для того, что уже лежит на месте, и для нового.
    pub dst_info: String,
    pub src_info: String,
    pub reply: mpsc::Sender<Answer>,
}

pub(super) struct Job {
    pub title: String,
    pub progress: Arc<JobProgress>,
    pub started: Instant,
    conflicts: mpsc::Receiver<ConflictRequest>,
    _task: Task<()>,
}

#[derive(Debug, Clone)]
pub(super) enum JobKind {
    Normal,
    /// Вставка; после успешного переноса буфер очищается, как в Thunar.
    Paste { cut: bool },
    /// Отмена этой записи; сама в историю не попадает.
    Undo(Record),
}

struct UiAsker(mpsc::Sender<ConflictRequest>);

impl Asker for UiAsker {
    fn ask(&self, c: &Conflict) -> Answer {
        let cancel = Answer { resolution: Resolution::Cancel, apply_to_all: false };
        let (tx, rx) = mpsc::channel();
        let req = ConflictRequest { conflict: c.clone(), dst_info: describe(&c.dst), src_info: describe(&c.src), reply: tx };
        if self.0.send(req).is_err() {
            return cancel;
        }
        // Диалог закрыли, не ответив (или окно закрылось) — считаем отменой.
        rx.recv().unwrap_or(cancel)
    }
}

/// «Размер: 1.5 МБ · Изменён: 12.11.2026 10:41» (для папки — число элементов).
fn describe(path: &Path) -> String {
    let Ok(meta) = fs::symlink_metadata(path) else { return String::new() };
    let what = if meta.is_dir() {
        let n = fs::read_dir(path).map(|d| d.count()).unwrap_or(0);
        format!("Папка, элементов: {n}")
    } else if meta.file_type().is_symlink() {
        let target = fs::read_link(path).map(|t| t.display().to_string()).unwrap_or_default();
        format!("Ссылка → {target}")
    } else {
        format!("Размер: {}", human_size(meta.len()))
    };
    format!("{what}  ·  Изменён: {}", ops::format_mtime(&meta))
}

impl FilesView {
    pub(super) fn run_cmd(&mut self, cmd: Cmd, cx: &mut Context<Self>) {
        self.menu = None;
        if self.tree.is_none() {
            return;
        }
        match cmd {
            Cmd::Open => {
                if let Some(tree) = &self.tree {
                    let _ = std::process::Command::new("xdg-open").arg(tree.path(self.selected)).spawn();
                }
            }
            Cmd::Cut | Cmd::Copy => {
                let paths = self.target_paths();
                if paths.is_empty() {
                    return self.notice("Нечего копировать", cx);
                }
                let mode = if cmd == Cmd::Cut { Mode::Move } else { Mode::Copy };
                cx.write_to_clipboard(ClipboardItem::new_string(join_paths(&paths)));
                let verb = if mode == Mode::Move { "Вырезано" } else { "Скопировано" };
                self.notice(format!("{verb} в буфер: {}", count_label(&paths)), cx);
                self.clipboard = Some(Clip { mode, paths });
            }
            Cmd::Paste => self.paste(cx),
            Cmd::Rename => {
                let targets = self.targets();
                let [id] = targets[..] else {
                    return self.notice("Переименовать можно только один элемент", cx);
                };
                let tree = self.tree.as_ref().unwrap();
                let n = tree.node(id);
                // Как в Thunar: выделено имя без расширения.
                let base = ops::split_ext(n.name.as_bytes(), n.is_dir).0.len();
                self.dialog = Some(Dialog::Name {
                    purpose: NamePurpose::Rename(tree.path(id)),
                    field: TextField::new(n.name.to_string(), 0..base),
                    error: None,
                });
            }
            Cmd::NewFolder | Cmd::NewFile => {
                let is_dir = cmd == Cmd::NewFolder;
                let (_, parent) = self.dest_dir();
                let default = if is_dir { "Новая папка" } else { "Новый файл" };
                let name = ops::next_new_name(&parent, default, is_dir);
                let len = name.len();
                self.dialog = Some(Dialog::Name {
                    purpose: NamePurpose::Create { parent, is_dir },
                    field: TextField::new(name, 0..len),
                    error: None,
                });
            }
            Cmd::Trash => {
                let paths = self.target_paths();
                if paths.is_empty() {
                    return;
                }
                self.run_job("Перемещение в корзину", JobKind::Normal, cx, move |p, _| ops::trash_all(&paths, p));
            }
            Cmd::Delete => {
                let paths = self.target_paths();
                if !paths.is_empty() {
                    self.dialog = Some(Dialog::ConfirmDelete { paths });
                }
            }
            Cmd::Undo => {
                if self.job.is_some() {
                    return self.notice("Дождитесь окончания текущей операции", cx);
                }
                let Some(rec) = self.undo.pop() else {
                    return self.notice("Нечего отменять", cx);
                };
                let title = format!("Отмена: {}", rec.describe());
                let r = rec.clone();
                self.run_job(title, JobKind::Undo(rec), cx, move |p, _| ops::undo(&r, p));
            }
            Cmd::CopyPath => {
                let paths = self.target_paths();
                if !paths.is_empty() {
                    cx.write_to_clipboard(ClipboardItem::new_string(join_paths(&paths)));
                    self.notice("Путь скопирован", cx);
                }
            }
        }
        cx.notify();
    }

    fn paste(&mut self, cx: &mut Context<Self>) {
        // Свой буфер, а если он пуст — пути из системного (например, из терминала).
        let clip = self.clipboard.clone().or_else(|| {
            let text = cx.read_from_clipboard()?.text()?;
            let paths = parse_paths(&text)?;
            Some(Clip { mode: Mode::Copy, paths })
        });
        let Some(clip) = clip else {
            return self.notice("Буфер обмена пуст", cx);
        };
        let (_, mut dest) = self.dest_dir();
        // Ctrl+C, Ctrl+V на той же папке — это дубликат рядом с ней, а не копия внутрь себя.
        if let Some(p) = clip.paths.iter().find(|s| dest.starts_with(s)).and_then(|s| s.parent()) {
            dest = p.to_path_buf();
        }
        let title = if clip.mode == Mode::Move { "Перемещение" } else { "Копирование" };
        let cut = clip.mode == Mode::Move;
        self.run_job(title, JobKind::Paste { cut }, cx, move |p, a| ops::transfer(clip.mode, &clip.paths, &dest, p, a));
    }

    /// Можно ли сейчас вставить (для пункта меню).
    pub(super) fn can_paste(&self) -> bool {
        self.clipboard.is_some()
    }

    /// Элементы, над которыми работает команда: отмеченные или выделенный.
    /// Корень не входит; если отмечены и папка, и что-то внутри неё — берётся только папка.
    pub(super) fn targets(&self) -> Vec<NodeId> {
        let Some(tree) = &self.tree else { return Vec::new() };
        let mut ids: Vec<NodeId> = if self.marked.is_empty() {
            vec![self.selected]
        } else {
            self.marked.iter().copied().collect()
        };
        ids.retain(|&id| id != Tree::ROOT && tree.contains(id));
        let set: std::collections::HashSet<NodeId> = ids.iter().copied().collect();
        ids.retain(|&id| !tree.ancestors(id).iter().any(|a| *a != id && set.contains(a)));
        ids.sort_by_cached_key(|&id| tree.path(id));
        ids
    }

    pub(super) fn target_paths(&self) -> Vec<PathBuf> {
        let Some(tree) = &self.tree else { return Vec::new() };
        self.targets().into_iter().map(|id| tree.path(id)).collect()
    }

    /// Куда вставлять и создавать: выделенная папка или папка выделенного файла.
    pub(super) fn dest_dir(&self) -> (NodeId, PathBuf) {
        let tree = self.tree.as_ref().expect("дерево есть");
        let n = tree.node(self.selected);
        let id = if n.is_dir { self.selected } else { n.parent.unwrap_or(Tree::ROOT) };
        (id, tree.path(id))
    }

    pub(super) fn notice(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        self.notice = Some((text.into(), Instant::now()));
        cx.notify();
        cx.spawn(async move |this, cx| {
            Timer::after(NOTICE_TTL).await;
            let _ = this.update(cx, |_, cx| cx.notify());
        })
        .detach();
    }

    pub(super) fn current_notice(&self) -> Option<&str> {
        self.notice.as_ref().filter(|(_, at)| at.elapsed() < NOTICE_TTL).map(|(s, _)| s.as_str())
    }

    pub(super) fn cancel_job(&mut self) {
        if let Some(job) = &self.job {
            job.progress.cancel();
        }
    }

    // ---------- фоновые операции ----------

    pub(super) fn run_job(
        &mut self,
        title: impl Into<String>,
        kind: JobKind,
        cx: &mut Context<Self>,
        work: impl FnOnce(&JobProgress, &dyn Asker) -> Outcome + Send + 'static,
    ) {
        if self.job.is_some() {
            if let JobKind::Undo(rec) = kind {
                self.undo.push(rec);
            }
            return self.notice("Дождитесь окончания текущей операции", cx);
        }
        let progress = Arc::new(JobProgress::default());
        let (tx, rx) = mpsc::channel();
        let p = progress.clone();
        let bg = cx.background_executor().spawn(async move {
            let asker = UiAsker(tx);
            let out = work(&p, &asker);
            drop(asker);
            // Затронутые пути перечитываем здесь же, в фоне: папки бывают большими.
            let refresh = rescan(&out.touched);
            (out, refresh)
        });
        let task = cx.spawn(async move |this, cx| {
            let (out, refresh) = bg.await;
            let _ = this.update(cx, |this, cx| {
                this.finish_job(kind, out, refresh, cx);
                cx.notify();
            });
        });
        self.job = Some(Job { title: title.into(), progress, started: Instant::now(), conflicts: rx, _task: task });

        // Пока операция идёт: перерисовываем прогресс и забираем вопросы о конфликтах.
        cx.spawn(async move |this, cx| {
            loop {
                Timer::after(Duration::from_millis(100)).await;
                let running = this
                    .update(cx, |this, cx| {
                        let Some(job) = &this.job else { return false };
                        if this.dialog.is_none() {
                            if let Ok(req) = job.conflicts.try_recv() {
                                this.dialog = Some(Dialog::Conflict { req, apply_all: false });
                            }
                        }
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !running {
                    break;
                }
            }
        })
        .detach();
        cx.notify();
    }

    fn finish_job(&mut self, kind: JobKind, out: Outcome, refresh: Vec<(PathBuf, Option<RawNode>)>, cx: &mut Context<Self>) {
        let title = self.job.take().map(|j| j.title).unwrap_or_default();
        if matches!(self.dialog, Some(Dialog::Conflict { .. })) {
            self.dialog = None;
        }
        let select = match (&kind, &out.record) {
            (JobKind::Undo(r), _) => restored_paths(r),
            (_, Some(r)) => new_paths(r),
            _ => Vec::new(),
        };
        self.apply_refresh(refresh, &select);

        if let JobKind::Paste { cut: true } = kind {
            if out.record.is_some() {
                self.clipboard = None;
            }
        }
        if !matches!(kind, JobKind::Undo(_)) {
            if let Some(r) = out.record {
                self.undo.push(r);
                if self.undo.len() > UNDO_LIMIT {
                    self.undo.remove(0);
                }
            }
        }
        if !out.errors.is_empty() {
            self.dialog = Some(Dialog::Errors { title: format!("{title}: не всё получилось"), errors: out.errors });
        } else if out.cancelled {
            self.notice(format!("{title}: прервано"), cx);
        } else {
            self.notice(format!("{title}: готово"), cx);
        }
    }

    /// Заменяет в дереве затронутые узлы свежими данными с диска, сохраняя
    /// выделение, раскрытые папки, корень карты и фильтр типа.
    fn apply_refresh(&mut self, refresh: Vec<(PathBuf, Option<RawNode>)>, select: &[PathBuf]) {
        let Some(arc) = self.tree.as_mut() else { return };
        let old: &Tree = arc;
        let sel_path = old.path(self.selected);
        let view_path = old.path(self.view_root);
        let expanded: Vec<PathBuf> =
            self.expanded.iter().filter(|&&id| old.contains(id)).map(|&id| old.path(id)).collect();
        let marked: Vec<PathBuf> =
            self.marked.iter().filter(|&&id| old.contains(id)).map(|&id| old.path(id)).collect();
        let filter_ext = self.type_filter.and_then(|k| old.types.get(k as usize)).map(|t| t.ext.clone());

        let tree = Arc::make_mut(arc);
        for (path, raw) in refresh {
            if let Some(id) = tree.find(&path) {
                if id == Tree::ROOT {
                    continue;
                }
                tree.remove(id);
            }
            if let (Some(raw), Some(parent)) = (raw, path.parent().and_then(|p| tree.find(p))) {
                tree.insert(parent, raw);
            }
        }

        let tree = self.tree.clone().unwrap();
        self.expanded = expanded.iter().filter_map(|p| tree.find(p)).collect();
        self.expanded.insert(Tree::ROOT);
        self.view_root = nearest(&tree, &view_path);
        if !tree.node(self.view_root).is_dir {
            self.view_root = tree.node(self.view_root).parent.unwrap_or(Tree::ROOT);
        }
        self.selected = nearest(&tree, &sel_path);
        self.marked = marked.iter().filter_map(|p| tree.find(p)).collect();
        self.type_filter =
            filter_ext.and_then(|e| tree.types.iter().position(|t| t.ext == e)).map(|i| i as u16);
        self.invalidate_map();

        let new: Vec<NodeId> = select.iter().filter_map(|p| tree.find(p)).collect();
        if let Some(&first) = new.first() {
            self.marked = if new.len() > 1 { new.iter().copied().collect() } else { Default::default() };
            self.reveal(first);
        } else {
            self.rebuild_rows();
        }
    }
}

/// Свежие данные с диска для затронутых путей. Если в списке есть и папка,
/// и что-то внутри неё, перечитывается только папка.
fn rescan(touched: &[PathBuf]) -> Vec<(PathBuf, Option<RawNode>)> {
    touched
        .iter()
        .filter(|p| !touched.iter().any(|q| q != *p && p.starts_with(q)))
        .map(|p| {
            let raw = scan(p, &ScanOptions::default(), &Progress::default()).ok().map(|mut r| {
                r.name = ops::display_name(p).into();
                r
            });
            (p.clone(), raw)
        })
        .collect()
}

/// Ближайший существующий узел по пути (сам путь или его предок).
fn nearest(tree: &Tree, path: &Path) -> NodeId {
    let mut cur = Some(path);
    while let Some(p) = cur {
        if let Some(id) = tree.find(p) {
            return id;
        }
        cur = p.parent();
    }
    Tree::ROOT
}

/// Что выделить после операции: то, что появилось.
fn new_paths(r: &Record) -> Vec<PathBuf> {
    match r {
        Record::Copied(v) => v.clone(),
        Record::Moved(v) => v.iter().map(|(_, to)| to.clone()).collect(),
        Record::Renamed { to, .. } => vec![to.clone()],
        Record::Created(p) => vec![p.clone()],
        Record::Trashed(_) => Vec::new(),
    }
}

/// Что выделить после отмены: то, что вернулось на место.
fn restored_paths(r: &Record) -> Vec<PathBuf> {
    match r {
        Record::Moved(v) => v.iter().map(|(from, _)| from.clone()).collect(),
        Record::Renamed { from, .. } => vec![from.clone()],
        Record::Trashed(items) => items.iter().map(|i| i.original.clone()).collect(),
        Record::Copied(_) | Record::Created(_) => Vec::new(),
    }
}

fn join_paths(paths: &[PathBuf]) -> String {
    paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("\n")
}

fn count_label(paths: &[PathBuf]) -> String {
    match paths {
        [one] => format!("«{}»", ops::display_name(one)),
        _ => format!("{} элементов", paths.len()),
    }
}

/// Пути из текста буфера: по одному на строку, абсолютные или `file://`, все существуют.
fn parse_paths(text: &str) -> Option<Vec<PathBuf>> {
    let mut out = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let p = match line.strip_prefix("file://") {
            Some(rest) => {
                use std::os::unix::ffi::OsStringExt;
                PathBuf::from(std::ffi::OsString::from_vec(percent_decode(rest)))
            }
            None => PathBuf::from(line),
        };
        if !p.is_absolute() || fs::symlink_metadata(&p).is_err() {
            return None;
        }
        out.push(p);
    }
    (!out.is_empty()).then_some(out)
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
    fn parses_clipboard_paths() {
        assert_eq!(percent_decode("/tmp/a%20b"), b"/tmp/a b");
        assert_eq!(percent_decode("/x%2"), b"/x%2");
        assert_eq!(parse_paths("/\nfile:///tmp\n"), Some(vec![PathBuf::from("/"), PathBuf::from("/tmp")]));
        assert_eq!(parse_paths("просто текст"), None);
        assert_eq!(parse_paths(""), None);
    }
}
