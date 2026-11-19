//! Нижняя панель (`Ctrl+I`, «Свойства» — `Alt+Enter`).
//!
//! - Папка или диск: treemap, типы файлов и самые крупные файлы. Сканируется текущая
//!   папка; одно дерево обслуживает все её подпапки, поэтому переход внутрь мгновенный.
//!   После операций с файлами дерево не пересканируется целиком, а точечно обновляется.
//! - Файл: свойства и предпросмотр (картинка или начало текста).
//! - Корзина: сколько в ней и сколько места.

use std::cell::RefCell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use conchy_files::analysis::{self, Summary};
use conchy_files::cache::DiskCache;
use conchy_files::layout::{self, Cell, CellKind, HEADER_H};
use conchy_files::model::{NodeId, Tree, human_size};
use conchy_files::scan::{Progress, ScanOptions, scan};
use conchy_files::treemap::Rect;
use conchy_files::{dir, icons, mime, ops};
use gpui::{
    AnyElement, App, Bounds, ClickEvent, ContentMask, Context, CursorStyle, Hsla, MouseButton, MouseDownEvent,
    MouseMoveEvent, ObjectFit, Pixels, Task, TextRun, Timer, Window, canvas, div, hsla, img, linear_color_stop,
    linear_gradient, point, prelude::*, px, quad, rgb, size,
};

use super::{ACCENT, BG, BORDER, DANGER, DIM, FilesWindow, PANEL, TEXT, items_label};

const MAP_BG: u32 = 0x101018;
/// Сколько просканированных деревьев держать в памяти (дерево домашней папки — сотни МБ).
const KEEP_TREES: usize = 2;
/// Изменённые деревья пишутся на диск один раз, когда изменения затихли на это время.
const SAVE_DELAY: Duration = Duration::from_secs(10);
const SIDE_W: f32 = 310.0;
const TOP_N: usize = 8;
const PREVIEW_BYTES: usize = 16 * 1024;

pub(super) struct Analysis {
    pub tree: Arc<Tree>,
    /// Файловая система корня. Сканер не заходит на другие ФС, поэтому дерево
    /// покрывает только пути на этой же: `/home` на отдельном подтоме btrfs в дереве
    /// `/` — лишь пустая заглушка.
    pub dev: u64,
    /// Когда сделан снимок (секунды Unix); у дерева из кеша — время того сканирования.
    pub scanned_at: i64,
    /// Сколько заняло полное сканирование корня.
    pub took: Duration,
    pub errors: u64,
    /// Папки, проверенные в этом сеансе: путь, когда, сколько заняло.
    /// Всё остальное в дереве — из кеша и могло устареть.
    pub fresh: Vec<(PathBuf, Instant, Duration)>,
    /// Изменилось после последней записи на диск.
    pub dirty: bool,
}

impl Analysis {
    /// Самая глубокая проверка этого сеанса, покрывающая `p`.
    fn fresh_for(&self, p: &Path) -> Option<&(PathBuf, Instant, Duration)> {
        self.fresh.iter().filter(|(f, ..)| p.starts_with(f)).max_by_key(|(f, ..)| f.as_os_str().len())
    }

    /// Стоит ли хранить на диске: пересчитывать его заметно долго.
    fn worth_caching(&self) -> bool {
        self.took >= Duration::from_secs(1) || self.tree.node(Tree::ROOT).files >= 20_000
    }
}

/// Результат фонового сканирования.
enum ScanResult {
    /// Новое дерево с корнем в просканированной папке.
    New(Tree),
    /// Папка внутри уже имеющегося дерева: свежие данные вклеены в его копию.
    Patched { tree: Tree, base_root: PathBuf, generation: u64 },
}

pub(super) struct Scanning {
    pub root: PathBuf,
    pub progress: Arc<Progress>,
    pub started: Instant,
    _task: Task<()>,
}

#[derive(Default)]
struct MapCache {
    key: Option<(usize, u64, NodeId, [i32; 4])>,
    cells: Vec<Cell>,
}

#[derive(Default)]
pub(super) struct PanelState {
    pub done: Vec<Analysis>,
    pub scanning: Option<Scanning>,
    pub failed: Option<(PathBuf, String)>,
    /// Растёт при каждом изменении деревьев: сбрасывает кеши раскладки и сводки.
    pub generation: u64,
    summary: RefCell<Option<((usize, u64, NodeId), Rc<Summary>)>>,
    map: Rc<RefCell<MapCache>>,
    hovered: Option<Cell>,
    /// Ячейка, выбранная щелчком на карте (двойной щелчок — показать в списке).
    picked: Option<NodeId>,
    /// Перетаскивание края: (y начала, высота в начале).
    pub drag: Option<(f32, f32)>,
    preview: Option<(PathBuf, Option<String>)>,
    _preview_task: Option<Task<()>>,
    /// Дисковый кеш (`None` — выключен в `files.kdl`).
    pub cache: Option<Arc<DiskCache>>,
    /// Идёт чтение из кеша для этой папки.
    loading: Option<(PathBuf, Task<()>)>,
    /// Папки, для которых в кеше ничего нет (чтобы не искать снова).
    misses: HashSet<PathBuf>,
    /// Номер последней отложенной записи: более ранние таймеры ничего не делают.
    save_ticket: u64,
}

/// Что показывает панель.
enum PanelTarget {
    File(usize),
    Dir(PathBuf),
    Trash,
    Nothing,
}

impl FilesWindow {
    fn panel_target(&self) -> PanelTarget {
        if self.in_trash() {
            return PanelTarget::Trash;
        }
        let sel = self.selected_rows();
        if let [ix] = sel[..] {
            let e = &self.rows[ix].entry;
            return if e.is_dir && !e.is_symlink { PanelTarget::Dir(e.path.clone()) } else { PanelTarget::File(ix) };
        }
        match self.current_dir() {
            Some(d) => PanelTarget::Dir(d.to_path_buf()),
            None => PanelTarget::Nothing,
        }
    }

    /// Дерево, в котором есть папка `p`, и её узел.
    fn analysis_for(&self, p: &Path) -> Option<(&Analysis, NodeId)> {
        let dev = dev_of(p)?;
        self.panel.done.iter().find_map(|a| {
            if a.dev != dev || !p.starts_with(analysis::root_path(&a.tree)) {
                return None;
            }
            a.tree.find(p).map(|id| (a, id))
        })
    }

    pub(super) fn toggle_panel(&mut self, cx: &mut Context<Self>) {
        self.config.panel_visible = !self.config.panel_visible;
        let v = self.config.panel_visible.to_string();
        self.save_settings(vec![("panel.visible", v)], cx);
        self.sync_panel(cx);
        cx.notify();
    }

    pub fn show_panel(&mut self, cx: &mut Context<Self>) {
        if !self.config.panel_visible {
            self.toggle_panel(cx);
        }
    }

    /// Запускает то, что нужно панели для текущего выделения: сканирование папки
    /// или чтение начала текстового файла. Можно звать сколько угодно раз.
    pub(super) fn sync_panel(&mut self, cx: &mut Context<Self>) {
        if !self.config.panel_visible || self.pick.is_some() {
            return;
        }
        match self.panel_target() {
            PanelTarget::File(ix) => self.ensure_preview(ix, cx),
            PanelTarget::Dir(_) => {
                // Работаем с текущей папкой: её дерево покрывает и выделенные подпапки.
                let Some(cwd) = self.current_dir().map(Path::to_path_buf) else { return };
                if let Some(s) = &self.panel.scanning {
                    if cwd.starts_with(&s.root) {
                        return;
                    }
                }
                if let Some((a, _)) = self.analysis_for(&cwd) {
                    // Данные из кеша уже на экране; перепроверяем только эту папку.
                    if a.fresh_for(&cwd).is_none() {
                        self.start_scan(cwd, cx);
                    }
                    return;
                }
                if self.panel.loading.as_ref().is_some_and(|(p, _)| *p == cwd) {
                    return;
                }
                if self.panel.failed.as_ref().is_some_and(|(p, _)| *p == cwd) {
                    return;
                }
                if self.panel.cache.is_some() && !self.panel.misses.contains(&cwd) {
                    return self.load_from_cache(cwd, cx);
                }
                self.start_scan(cwd, cx);
            }
            PanelTarget::Trash | PanelTarget::Nothing => {}
        }
    }

    /// Включает или выключает дисковый кеш по настройкам и убирает в нём (в фоне).
    /// Выключенный кеш удаляется целиком, чтобы не занимать место.
    pub(super) fn configure_cache(&mut self, cx: &mut Context<Self>) {
        self.panel.misses.clear();
        if !self.config.cache_enabled {
            self.panel.cache = None;
            cx.background_executor().spawn(async { DiskCache::clear(&DiskCache::default_dir()) }).detach();
            return;
        }
        let cache = Arc::new(DiskCache::new(
            DiskCache::default_dir(),
            self.config.cache_max_mb as u64 * (1 << 20),
            Duration::from_secs(self.config.cache_max_age_days as u64 * 86_400),
        ));
        self.panel.cache = Some(cache.clone());
        cx.background_executor().spawn(async move { cache.cleanup() }).detach();
    }

    /// Ищет в дисковом кеше дерево для `dir` или папки выше (в фоне).
    fn load_from_cache(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        let Some(cache) = self.panel.cache.clone() else { return };
        let job = cx.background_executor().spawn({
            let dir = dir.clone();
            async move { cache.load_nearest(&dir) }
        });
        let for_dir = dir.clone();
        let task = cx.spawn(async move |this, cx| {
            let found = job.await;
            let _ = this.update(cx, |this, cx| {
                if this.panel.loading.as_ref().map(|(p, _)| p) != Some(&for_dir) {
                    return;
                }
                this.panel.loading = None;
                match found {
                    Some(c) => {
                        let root = PathBuf::from(&*c.tree.node(Tree::ROOT).name);
                        // В памяти уже есть дерево, которое правда содержит эту папку, — оно не хуже кеша.
                        let dev = dev_of(&root).unwrap_or(0);
                        if this.analysis_for(&for_dir).is_none() {
                            this.panel.done.retain(|a| a.dev != dev || !analysis::root_path(&a.tree).starts_with(&root));
                            this.panel.done.insert(
                                0,
                                Analysis {
                                    dev,
                                    tree: Arc::new(c.tree),
                                    scanned_at: c.scanned_at,
                                    took: Duration::from_millis(c.took_ms),
                                    errors: c.errors,
                                    fresh: Vec::new(),
                                    dirty: false,
                                },
                            );
                            this.flush_overflow();
                            this.panel.done.truncate(KEEP_TREES);
                            this.panel.generation += 1;
                            this.apply_deep_sizes();
                        }
                    }
                    None => {
                        this.panel.misses.insert(for_dir.clone());
                    }
                }
                // Защита от круга «загрузили из кеша — а анализа для папки всё нет»: дальше — сканирование.
                if this.analysis_for(&for_dir).is_none() {
                    this.panel.misses.insert(for_dir.clone());
                }
                // Следующая отрисовка запустит перепроверку или обычное сканирование.
                cx.notify();
            });
        });
        self.panel.loading = Some((dir, task));
    }

    /// Сканирует `root`. Если папка лежит внутри уже имеющегося дерева (например,
    /// из кеша), свежие данные вклеиваются в него, а не строится отдельное дерево.
    pub(super) fn start_scan(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        if let Some(old) = self.panel.scanning.take() {
            old.progress.cancelled.store(true, Ordering::Relaxed);
        }
        self.panel.failed = None;
        let base = self
            .panel
            .done
            .iter()
            .find(|a| {
                // Вклеивать можно только в дерево той же файловой системы, где уже есть родитель.
                let r = analysis::root_path(&a.tree);
                root.starts_with(r)
                    && r != root
                    && dev_of(&root) == Some(a.dev)
                    && root.parent().is_some_and(|parent| a.tree.find(parent).is_some())
            })
            .map(|a| (a.tree.clone(), self.panel.generation));
        let progress = Arc::new(Progress::default());
        let p = progress.clone();
        let r = root.clone();
        let job = cx.background_executor().spawn(async move {
            let t = Instant::now();
            let raw = scan(&r, &ScanOptions::default(), &p);
            let took = t.elapsed();
            let res = raw.map(|mut raw| match base {
                Some((tree, generation)) => {
                    // Копия дерева правится в фоне: интерфейс не ждёт.
                    let base_root = analysis::root_path(&tree).to_path_buf();
                    let mut copy = (*tree).clone();
                    raw.name = ops::display_name(&r).into();
                    analysis::patch(&mut copy, vec![(r.clone(), Some(raw))]);
                    ScanResult::Patched { tree: copy, base_root, generation }
                }
                None => ScanResult::New(Tree::from_raw(raw)),
            });
            (res, took)
        });
        let task_root = root.clone();
        let task = cx.spawn(async move |this, cx| {
            let (res, took) = job.await;
            let _ = this.update(cx, |this, cx| {
                let Some(s) = &this.panel.scanning else { return };
                if s.root != task_root || s.progress.cancelled.load(Ordering::Relaxed) {
                    return;
                }
                let errors = s.progress.errors.load(Ordering::Relaxed);
                this.panel.scanning = None;
                let mut changed = false;
                match res {
                    Ok(ScanResult::New(tree)) => {
                        // Деревья внутри нового больше не нужны: новое их покрывает.
                        let dev = dev_of(&task_root).unwrap_or(0);
                        this.panel.done.retain(|a| a.dev != dev || !analysis::root_path(&a.tree).starts_with(&task_root));
                        this.panel.done.insert(
                            0,
                            Analysis {
                                dev,
                                tree: Arc::new(tree),
                                scanned_at: unix_now(),
                                took,
                                errors,
                                fresh: vec![(task_root.clone(), Instant::now(), took)],
                                dirty: true,
                            },
                        );
                        // Вытесненное из памяти дерево могло не успеть записаться.
                        this.flush_overflow();
                        this.panel.done.truncate(KEEP_TREES);
                        changed = true;
                    }
                    Ok(ScanResult::Patched { tree, base_root, generation }) => {
                        // Пока шло сканирование, дерево менялось — результат устарел, перепроверим снова.
                        if generation == this.panel.generation {
                            if let Some(a) = this.panel.done.iter_mut().find(|a| analysis::root_path(&a.tree) == base_root) {
                                a.tree = Arc::new(tree);
                                a.fresh.retain(|(f, ..)| !f.starts_with(&task_root));
                                a.fresh.push((task_root.clone(), Instant::now(), took));
                                a.dirty = true;
                                changed = true;
                            }
                        }
                    }
                    Err(e) => this.panel.failed = Some((task_root.clone(), e.to_string())),
                }
                this.panel.generation += 1;
                this.panel.picked = None;
                this.apply_deep_sizes();
                if changed {
                    this.schedule_save(cx);
                }
                cx.notify();
            });
        });
        self.panel.scanning = Some(Scanning { root, progress, started: Instant::now(), _task: task });

        cx.spawn(async move |this, cx| {
            loop {
                Timer::after(Duration::from_millis(100)).await;
                let running = this
                    .update(cx, |this, cx| {
                        cx.notify();
                        this.panel.scanning.is_some()
                    })
                    .unwrap_or(false);
                if !running {
                    break;
                }
            }
        })
        .detach();
    }

    /// Ставит в колонку «Размер» у папок настоящий размер, если он уже известен.
    pub(super) fn apply_deep_sizes(&mut self) {
        let mut changed = false;
        for i in 0..self.rows.len() {
            if !self.rows[i].entry.is_dir || self.rows[i].entry.trash.is_some() {
                continue;
            }
            let size = self.analysis_for(&self.rows[i].entry.path).map(|(a, id)| a.tree.node(id).size);
            let row = &mut self.rows[i];
            if let Some(size) = size {
                if row.entry.deep_size != Some(size) {
                    row.entry.deep_size = Some(size);
                    row.size = human_size(size);
                    changed = true;
                }
            }
        }
        if changed && self.sort.key == dir::SortKey::Size {
            dir::sort(&mut self.rows, self.sort);
            self.rebuild_visible();
        }
    }

    /// После операции обновляет затронутые места в просканированных деревьях (в фоне).
    pub(super) fn patch_analyses(&mut self, touched: Vec<PathBuf>, cx: &mut Context<Self>) {
        let relevant: Vec<PathBuf> = touched
            .into_iter()
            .filter(|p| self.panel.done.iter().any(|a| p.starts_with(analysis::root_path(&a.tree))))
            .collect();
        if relevant.is_empty() {
            return;
        }
        let job = cx.background_executor().spawn(async move { analysis::rescan(&relevant) });
        cx.spawn(async move |this, cx| {
            let refresh = job.await;
            let _ = this.update(cx, |this, cx| {
                let mut changed = false;
                for a in &mut this.panel.done {
                    // Только изменения на той же файловой системе, что и дерево.
                    let mine: Vec<_> = refresh
                        .iter()
                        .filter(|(p, _)| p.parent().and_then(dev_of) == Some(a.dev))
                        .cloned()
                        .collect();
                    if !mine.is_empty() && analysis::patch(Arc::make_mut(&mut a.tree), mine) {
                        a.dirty = true;
                        changed = true;
                    }
                }
                if changed {
                    this.panel.generation += 1;
                    this.panel.picked = None;
                    this.panel.hovered = None;
                    this.apply_deep_sizes();
                    this.schedule_save(cx);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Откладывает запись изменённых деревьев: серия сканирований и операций
    /// даёт одну запись, когда всё затихнет на [`SAVE_DELAY`].
    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        self.panel.save_ticket += 1;
        let ticket = self.panel.save_ticket;
        cx.spawn(async move |this, cx| {
            Timer::after(SAVE_DELAY).await;
            let _ = this.update(cx, |this, cx| {
                if this.panel.save_ticket == ticket {
                    this.flush_cache(cx);
                }
            });
        })
        .detach();
    }

    /// Записывает изменённые деревья в фоне. Неизменённые не трогает.
    fn flush_cache(&mut self, cx: &mut Context<Self>) {
        for job in self.take_dirty() {
            cx.background_executor().spawn(async move { job() }).detach();
        }
    }

    /// То же при закрытии окна: синхронно, иначе процесс завершится раньше записи.
    pub(super) fn flush_cache_now(&mut self) {
        for job in self.take_dirty() {
            job();
        }
    }

    /// Деревья, которые сейчас вытеснятся из памяти, записываем, пока не потеряли.
    fn flush_overflow(&mut self) {
        let Some(cache) = self.panel.cache.clone() else { return };
        for a in self.panel.done.iter_mut().skip(KEEP_TREES).filter(|a| a.dirty) {
            a.dirty = false;
            if a.worth_caching() {
                let (tree, at, took, errors) = (a.tree.clone(), a.scanned_at, a.took.as_millis() as u64, a.errors);
                let cache = cache.clone();
                std::thread::spawn(move || save_one(&cache, &tree, at, took, errors));
            }
        }
    }

    /// Снимает отметки «изменено» и возвращает задания записи для деревьев,
    /// которые стоит хранить (мелкие быстрее пересчитать, чем читать).
    fn take_dirty(&mut self) -> Vec<Box<dyn FnOnce() + Send>> {
        let Some(cache) = self.panel.cache.clone() else { return Vec::new() };
        let mut jobs: Vec<Box<dyn FnOnce() + Send>> = Vec::new();
        for a in self.panel.done.iter_mut().filter(|a| a.dirty) {
            a.dirty = false;
            if !a.worth_caching() {
                continue;
            }
            let (tree, at, took, errors) = (a.tree.clone(), a.scanned_at, a.took.as_millis() as u64, a.errors);
            let cache = cache.clone();
            jobs.push(Box::new(move || save_one(&cache, &tree, at, took, errors)));
        }
        jobs
    }

    fn ensure_preview(&mut self, ix: usize, cx: &mut Context<Self>) {
        let e = &self.rows[ix].entry;
        if !is_texty(&e.mime) || e.broken {
            return;
        }
        if self.panel.preview.as_ref().is_some_and(|(p, _)| *p == e.path) {
            return;
        }
        let path = e.path.clone();
        self.panel.preview = Some((path.clone(), None));
        let job = cx.background_executor().spawn({
            let path = path.clone();
            async move { read_head(&path) }
        });
        self.panel._preview_task = Some(cx.spawn(async move |this, cx| {
            let text = job.await;
            let _ = this.update(cx, |this, cx| {
                if this.panel.preview.as_ref().is_some_and(|(p, _)| *p == path) {
                    this.panel.preview = Some((path, Some(text)));
                    cx.notify();
                }
            });
        }));
    }

    /// Показывает элемент в списке: выделяет его, при необходимости перейдя в его папку.
    fn reveal_path(&mut self, p: PathBuf, cx: &mut Context<Self>) {
        let Some(parent) = p.parent().map(Path::to_path_buf) else { return };
        if self.current_dir() == Some(parent.as_path()) {
            if let Some(pos) = self.visible.iter().position(|&i| self.rows[i].entry.path == p) {
                self.click_row(pos, Default::default());
                self.list_scroll.scroll_to_item(pos, gpui::ScrollStrategy::Center);
            }
        } else {
            self.back.push(self.location.clone());
            self.forward.clear();
            self.set_location(conchy_files::places::Target::Dir(parent), vec![p], cx);
        }
        cx.notify();
    }

    pub(super) fn render_panel(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.config.panel_visible {
            return None;
        }
        let body = match self.panel_target() {
            PanelTarget::File(ix) => self.render_file_panel(ix, cx),
            PanelTarget::Dir(p) => self.render_dir_panel(&p, cx),
            PanelTarget::Trash => self.render_trash_panel(),
            PanelTarget::Nothing => placeholder("Нечего показать"),
        };
        Some(
            div()
                .flex()
                .flex_col()
                .h(px(self.config.panel_height as f32))
                .flex_shrink_0()
                .bg(rgb(PANEL))
                .border_t_1()
                .border_color(rgb(BORDER))
                .child(
                    // Край панели: тянуть, чтобы изменить высоту.
                    div()
                        .id("panel-handle")
                        .h(px(5.))
                        .w_full()
                        .flex_shrink_0()
                        .cursor(CursorStyle::ResizeRow)
                        .hover(|s| s.bg(rgb(BORDER)))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                                cx.stop_propagation();
                                this.panel.drag = Some((f32::from(ev.position.y), this.config.panel_height as f32));
                            }),
                        ),
                )
                .child(div().flex_1().min_h_0().child(body))
                .into_any_element(),
        )
    }

    fn render_dir_panel(&self, p: &Path, cx: &mut Context<Self>) -> AnyElement {
        let mount = self.mounts.iter().find(|m| m.mount_point == p);
        let Some((a, node)) = self.analysis_for(p) else {
            if let Some(s) = &self.panel.scanning {
                let pr = &s.progress;
                return div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .text_color(rgb(DIM))
                    .child(div().text_color(rgb(TEXT)).child(format!("Анализ «{}»…", ops::display_name(&s.root))))
                    .child(format!(
                        "{} файлов · {} папок · {} · {:.1} с",
                        pr.files.load(Ordering::Relaxed),
                        pr.dirs.load(Ordering::Relaxed),
                        human_size(pr.bytes.load(Ordering::Relaxed)),
                        s.started.elapsed().as_secs_f32()
                    ))
                    .into_any_element();
            }
            if self.panel.loading.is_some() {
                return placeholder("Загрузка анализа из кеша…");
            }
            if let Some((_, e)) = &self.panel.failed {
                return placeholder(&format!("Не удалось проанализировать: {e}"));
            }
            return placeholder("Анализ появится, когда папка будет просканирована");
        };

        let tree = &a.tree;
        let summary = self.summary(a, node);
        let n = tree.node(node);

        let title = match mount {
            Some(m) => m.name.clone(),
            None if p.parent().is_none() => "Файловая система".into(),
            None => ops::display_name(p),
        };
        let facts = match self.panel.hovered.as_ref().map(|c| &c.kind) {
            Some(CellKind::Node(id)) => describe_node(tree, *id),
            Some(CellKind::Others { count, size, .. }) => format!("ещё {} мелких элементов · {}", count, human_size(*size)),
            None => match self.panel.picked.filter(|id| tree.contains(*id)) {
                Some(id) => describe_node(tree, id),
                None => format!(
                    "{} · на диске {} · {} файлов · {} папок",
                    human_size(n.size),
                    human_size(n.alloc),
                    summary.files,
                    summary.dirs
                ),
            },
        };
        // Свежая проверка — сколько заняла; данные из кеша — от какого числа.
        let mut meta = match a.fresh_for(p) {
            Some((_, at, took)) => {
                let age = at.elapsed().as_secs();
                let age = if age < 60 { "только что".to_string() } else { format!("{} мин назад", age / 60) };
                format!("за {:.2} с, {age}", took.as_secs_f32())
            }
            None => {
                let checking = self.panel.scanning.as_ref().is_some_and(|s| p.starts_with(&s.root));
                let date = dir::format_date(a.scanned_at, unix_now());
                format!("данные от {date}{}", if checking { " · проверяется…" } else { "" })
            }
        };
        if a.errors > 0 {
            meta.push_str(&format!(" · нет доступа: {}", a.errors));
        }
        let root = p.to_path_buf();
        let header = div()
            .flex()
            .items_center()
            .gap_3()
            .px_3()
            .h(px(30.))
            .flex_shrink_0()
            .child(div().flex_shrink_0().text_color(rgb(TEXT)).font_weight(gpui::FontWeight::SEMIBOLD).child(title))
            .child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().text_color(rgb(DIM)).child(facts))
            .child(div().flex_shrink_0().text_color(rgb(DIM)).text_size(px(11.)).child(meta))
            .child(
                small_button("panel-rescan", "Обновить").on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.start_scan(root.clone(), cx);
                    cx.notify();
                })),
            );

        let disk = mount.map(|m| {
            div()
                .flex()
                .items_center()
                .gap_3()
                .px_3()
                .pb_2()
                .flex_shrink_0()
                .text_color(rgb(DIM))
                .child(format!("{} · {}", m.device, m.fs_type))
                .child(div().w(px(220.)).child(usage(m.used_fraction())))
                .child(format!(
                    "занято {} из {}, свободно {}",
                    human_size(m.total.saturating_sub(m.free)),
                    human_size(m.total),
                    human_size(m.free)
                ))
        });

        div()
            .size_full()
            .flex()
            .flex_col()
            .child(header)
            .children(disk)
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_map(a, node, cx))
                    .child(self.render_side(a, &summary, cx)),
            )
            .into_any_element()
    }

    fn summary(&self, a: &Analysis, node: NodeId) -> Rc<Summary> {
        let key = (Arc::as_ptr(&a.tree) as usize, self.panel.generation, node);
        if let Some((k, s)) = self.panel.summary.borrow().as_ref() {
            if *k == key {
                return s.clone();
            }
        }
        let s = Rc::new(analysis::summarize(&a.tree, node, TOP_N));
        *self.panel.summary.borrow_mut() = Some((key, s.clone()));
        s
    }

    fn render_map(&self, a: &Analysis, node: NodeId, cx: &mut Context<Self>) -> AnyElement {
        let tree = a.tree.clone();
        let cache = self.panel.map.clone();
        let generation = self.panel.generation;
        let hovered = self.panel.hovered.as_ref().map(|c| c.rect);
        let mut marked: Vec<NodeId> = self.panel.picked.into_iter().collect();
        marked.extend(self.selected_rows().iter().filter_map(|&i| tree.find(&self.rows[i].entry.path)));

        let map = canvas(
            |_, _, _| {},
            move |bounds, _, window, cx| {
                window.paint_quad(quad(bounds, px(0.), rgb(MAP_BG), px(0.), rgb(MAP_BG), Default::default()));
                let b = to_rect(bounds);
                let key = (Arc::as_ptr(&tree) as usize, generation, node, [b.x as i32, b.y as i32, b.w as i32, b.h as i32]);
                let mut cache = cache.borrow_mut();
                if cache.key != Some(key) {
                    cache.cells = layout::layout(&tree, node, b.inset(1.0));
                    cache.key = Some(key);
                }
                window.with_content_mask(Some(ContentMask { bounds }), |window| {
                    for cell in &cache.cells {
                        paint_cell(&tree, cell, &marked, hovered, window, cx);
                    }
                });
            },
        )
        .size_full();

        div()
            .id("panel-map")
            .flex_1()
            .min_w_0()
            .h_full()
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
                let cell = this.cell_at(ev.position);
                if cell.as_ref().map(|c| c.rect) != this.panel.hovered.as_ref().map(|c| c.rect) {
                    this.panel.hovered = cell;
                    cx.notify();
                }
            }))
            .on_hover(cx.listener(|this, over: &bool, _, cx| {
                if !*over && this.panel.hovered.take().is_some() {
                    cx.notify();
                }
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                    let Some(cell) = this.cell_at(ev.position) else { return };
                    let CellKind::Node(id) = cell.kind else { return };
                    let Some(path) = this.map_tree().map(|t| t.path(id)) else { return };
                    let is_dir = this.map_tree().is_some_and(|t| t.node(id).is_dir);
                    if ev.click_count >= 2 {
                        // Двойной щелчок: папку открыть, файл показать в списке.
                        this.panel.picked = None;
                        if is_dir {
                            this.navigate(conchy_files::places::Target::Dir(path), cx);
                        } else {
                            this.reveal_path(path, cx);
                        }
                    } else {
                        this.panel.picked = Some(id);
                    }
                    cx.notify();
                }),
            )
            .child(map)
            .into_any_element()
    }

    /// Дерево, по которому сейчас нарисована карта.
    fn map_tree(&self) -> Option<Arc<Tree>> {
        match self.panel_target() {
            PanelTarget::Dir(p) => self.analysis_for(&p).map(|(a, _)| a.tree.clone()),
            _ => None,
        }
    }

    fn cell_at(&self, pos: gpui::Point<Pixels>) -> Option<Cell> {
        let cache = self.panel.map.borrow();
        layout::hit(&cache.cells, f32::from(pos.x), f32::from(pos.y)).cloned()
    }

    fn render_side(&self, a: &Analysis, s: &Summary, cx: &mut Context<Self>) -> AnyElement {
        let tree = &a.tree;
        let total = s.size.max(1) as f64;
        let heading = |t: &'static str| div().pt_2().pb_1().text_color(rgb(DIM)).text_size(px(11.)).child(t);
        let mut col = div()
            .id("panel-side")
            .w(px(SIDE_W))
            .flex_shrink_0()
            .h_full()
            .px_3()
            .overflow_y_scroll()
            .border_l_1()
            .border_color(rgb(BORDER))
            .child(heading("Типы файлов"));
        for t in s.types.iter().take(TOP_N) {
            let ext = &tree.types[t.kind as usize].ext;
            let label = if ext.is_empty() { "без расширения".to_string() } else { format!(".{ext}") };
            col = col.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .h(px(22.))
                    .child(div().size(px(10.)).flex_shrink_0().rounded_sm().bg(type_color(Some(t.kind))))
                    .child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().child(label))
                    .child(div().w(px(46.)).flex().justify_end().text_color(rgb(DIM)).child(format!("{:.0}%", t.size as f64 / total * 100.0)))
                    .child(div().w(px(72.)).flex().justify_end().child(human_size(t.size))),
            );
        }
        if s.types.len() > TOP_N {
            col = col.child(div().text_color(rgb(DIM)).child(format!("и ещё {} типов", s.types.len() - TOP_N)));
        }

        col = col.child(heading("Самые крупные файлы"));
        for (i, &id) in s.largest.iter().enumerate() {
            let n = tree.node(id);
            let path = tree.path(id);
            let where_ = path.parent().and_then(|p| p.strip_prefix(analysis::root_path(tree)).ok()).map(|p| p.display().to_string()).unwrap_or_default();
            col = col.child(
                div()
                    .id(("largest", i))
                    .flex()
                    .items_center()
                    .gap_2()
                    .h(px(if where_.is_empty() { 22. } else { 36. }))
                    .px_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .hover(|st| st.bg(rgb(BORDER)))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.reveal_path(path.clone(), cx)))
                    .child(div().size(px(10.)).flex_shrink_0().rounded_sm().bg(type_color(n.kind)))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .child(div().overflow_hidden().text_ellipsis().whitespace_nowrap().child(n.name.to_string()))
                            .when(!where_.is_empty(), |d| {
                                d.child(
                                    div()
                                        .overflow_hidden()
                                        .text_ellipsis()
                                        .whitespace_nowrap()
                                        .text_color(rgb(DIM))
                                        .text_size(px(11.))
                                        .child(where_),
                                )
                            }),
                    )
                    .child(div().w(px(72.)).flex_shrink_0().flex().justify_end().child(human_size(n.size))),
            );
        }
        col.into_any_element()
    }

    fn render_file_panel(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let row = &self.rows[ix];
        let e = &row.entry;
        let lmeta = std::fs::symlink_metadata(&e.path).ok();
        let meta = std::fs::metadata(&e.path).ok().or_else(|| lmeta.clone());

        let preview: AnyElement = if e.mime.starts_with("image/") && !e.broken {
            img(e.path.clone()).size_full().object_fit(ObjectFit::Contain).into_any_element()
        } else if let Some((p, Some(text))) = &self.panel.preview {
            if *p == e.path {
                let lines: Vec<String> = text.lines().take(300).map(|l| l.replace('\t', "    ")).collect();
                div()
                    .id("preview-text")
                    .size_full()
                    .p_2()
                    .overflow_y_scroll()
                    .bg(rgb(BG))
                    .rounded_md()
                    .font_family("monospace")
                    .text_size(px(12.))
                    .children(lines.into_iter().map(|l| div().whitespace_nowrap().child(if l.is_empty() { " ".into() } else { l })))
                    .into_any_element()
            } else {
                big_icon(row.icon.clone())
            }
        } else {
            big_icon(row.icon.clone())
        };

        let mut props: Vec<(&str, String)> = vec![("Имя", e.name.clone()), ("Тип", format!("{}  ({})", row.kind, e.mime))];
        if let Some(m) = &meta {
            use std::os::unix::fs::MetadataExt;
            props.push(("Размер", format!("{} ({} байт)", human_size(m.len()), group_digits(m.len()))));
            props.push(("На диске", human_size(m.blocks() * 512)));
            props.push(("Где", e.path.parent().map(|p| p.display().to_string()).unwrap_or_default()));
            props.push(("Изменён", ops::format_mtime(m)));
            if let Ok(acc) = m.accessed() {
                let secs = acc.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
                let now = unsafe { libc::time(std::ptr::null_mut()) } as i64;
                props.push(("Открыт", dir::format_date(secs, now)));
            }
            props.push(("Права", format!("{}  ({:o})", dir::mode_string(m.mode()), m.mode() & 0o7777)));
            props.push(("Владелец", format!("{} : {}", dir::user_name(m.uid()), dir::group_name(m.gid()))));
        }
        if e.is_symlink {
            let t = std::fs::read_link(&e.path).map(|t| t.display().to_string()).unwrap_or_default();
            props.push(("Ссылка на", if e.broken { format!("{t} (не существует)") } else { t }));
        }

        let perms = lmeta.as_ref().filter(|_| !e.is_symlink).map(|m| {
            use std::os::unix::fs::MetadataExt;
            permissions_grid(e.path.clone(), m.mode(), m.uid(), cx)
        });
        let grid = div()
            .flex()
            .flex_col()
            .gap_1()
            .p_3()
            .flex_1()
            .min_w_0()
            .children(props.into_iter().map(|(k, v)| {
                div()
                    .flex()
                    .gap_3()
                    .child(div().w(px(90.)).flex_shrink_0().text_color(rgb(DIM)).child(k))
                    .child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().child(v))
            }))
            .children(perms);

        div()
            .size_full()
            .flex()
            .child(div().w(gpui::relative(0.42)).max_w(px(560.)).h_full().p_3().flex().items_center().justify_center().child(preview))
            .child(grid)
            .into_any_element()
    }

    fn render_trash_panel(&self) -> AnyElement {
        let size: u64 = self.rows.iter().map(|r| r.entry.size).sum();
        let dirs = self.rows.iter().filter(|r| r.entry.is_dir).count();
        placeholder(&format!(
            "В корзине {} ({} — папки посчитаны без содержимого){}",
            items_label(self.rows.len() as u64),
            human_size(size),
            if dirs > 0 { format!(", папок: {dirs}") } else { String::new() }
        ))
    }
}

/// Номер файловой системы, на которой лежит путь.
fn dev_of(p: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(p).ok().map(|m| m.dev())
}

fn save_one(cache: &DiskCache, tree: &Tree, at: i64, took_ms: u64, errors: u64) {
    if let Err(e) = cache.save(tree, at, took_ms, errors) {
        eprintln!("conchy-files: кеш анализа не сохранён: {e}");
    }
}

fn unix_now() -> i64 {
    unsafe { libc::time(std::ptr::null_mut()) as i64 }
}

/// Права как в свойствах Thunar: владелец, группа, остальные × чтение, запись, выполнение.
/// Менять можно только свой файл (иначе chmod всё равно откажет) — тогда сетка просто показывает.
fn permissions_grid(path: PathBuf, mode: u32, owner: u32, cx: &mut Context<FilesWindow>) -> AnyElement {
    let mine = owner == unsafe { libc::getuid() } || unsafe { libc::getuid() } == 0;
    let mut grid = div().flex().flex_col().gap_1().pt_2().child(
        div()
            .flex()
            .gap_3()
            .text_color(rgb(DIM))
            .child(div().w(px(90.)).child(if mine { "Права" } else { "Права (только чтение)" }))
            .children(["чтение", "запись", "запуск"].map(|h| div().w(px(64.)).child(h))),
    );
    for (row, (label, shift)) in [("владелец", 6u32), ("группа", 3), ("остальные", 0)].into_iter().enumerate() {
        let mut line = div().flex().gap_3().items_center().child(div().w(px(90.)).text_color(rgb(DIM)).child(label));
        for (col, bit) in [4u32, 2, 1].into_iter().enumerate() {
            let mask = bit << shift;
            let on = mode & mask != 0;
            let p = path.clone();
            line = line.child(
                div().w(px(64.)).child(
                    div()
                        .id(("perm", row * 3 + col))
                        .size(px(16.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_sm()
                        .border_1()
                        .border_color(rgb(if on { ACCENT } else { DIM }))
                        .when(on, |d| d.bg(rgb(ACCENT)).text_color(rgb(BG)).child("✓"))
                        .when(mine, |d| {
                            d.cursor_pointer().on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                let new = (mode ^ mask) & 0o7777;
                                match std::fs::set_permissions(&p, std::os::unix::fs::PermissionsExt::from_mode(new)) {
                                    // Список перечитается сам (inotify), панель — при отрисовке.
                                    Ok(()) => cx.notify(),
                                    Err(e) => this.notice(format!("Права не изменены: {e}"), cx),
                                }
                            }))
                        }),
                ),
            );
        }
        grid = grid.child(line);
    }
    grid.into_any_element()
}

/// Текстовый ли тип: показываем начало файла.
fn is_texty(mime: &str) -> bool {
    mime.starts_with("text/")
        || mime.ends_with("+xml")
        || mime.ends_with("+json")
        || matches!(
            mime,
            "application/json"
                | "application/xml"
                | "application/x-shellscript"
                | "application/toml"
                | "application/x-yaml"
                | "application/yaml"
                | "application/javascript"
                | "application/x-desktop"
                | "application/sql"
                | "application/x-perl"
                | "application/x-ruby"
                | "application/x-php"
        )
        || mime == mime::TEXT
}

fn read_head(path: &Path) -> String {
    use std::io::Read;
    let mut buf = vec![0u8; PREVIEW_BYTES];
    let n = std::fs::File::open(path).and_then(|mut f| f.read(&mut buf)).unwrap_or(0);
    buf.truncate(n);
    String::from_utf8_lossy(&buf).into_owned()
}

fn describe_node(tree: &Tree, id: NodeId) -> String {
    let n = tree.node(id);
    let mut s = format!("{}  ·  {}  ·  {:.1}% от папки", tree.path(id).display(), human_size(n.size), tree.share_of_parent(id) * 100.0);
    if n.is_dir {
        s.push_str(&format!("  ·  {} файлов", n.files));
    }
    s
}

/// `1234567` → `1 234 567`.
fn group_digits(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

fn big_icon(icon: Option<PathBuf>) -> AnyElement {
    // Иконка 16 px из списка; для крупной ищем ту же в большом размере.
    let path = icon.and_then(|p| {
        let name = p.file_stem()?.to_string_lossy().into_owned();
        icons::icons().lookup(&[name], 96).or(Some(p))
    });
    match path {
        Some(p) => img(p).size(px(96.)).into_any_element(),
        None => div().into_any_element(),
    }
}

fn small_button(id: &'static str, label: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_2()
        .py_0p5()
        .flex_shrink_0()
        .rounded_md()
        .border_1()
        .border_color(rgb(BORDER))
        .cursor_pointer()
        .hover(|s| s.bg(rgb(BORDER)))
        .child(label)
}

fn usage(used: f32) -> impl IntoElement {
    let color = if used > 0.9 { rgb(DANGER) } else { rgb(ACCENT) };
    div()
        .relative()
        .w_full()
        .h(px(6.))
        .rounded_sm()
        .bg(rgb(BORDER))
        .child(div().absolute().top_0().left_0().h_full().rounded_sm().bg(color).w(gpui::relative(used.clamp(0.0, 1.0))))
}

fn placeholder(text: &str) -> AnyElement {
    div().size_full().flex().items_center().justify_center().text_color(rgb(DIM)).child(text.to_string()).into_any_element()
}

/// Свой цвет у каждого типа файлов, как в WizTree. Оттенки идут по золотому сечению,
/// поэтому соседние по размеру типы заметно различаются.
fn type_color(kind: Option<u16>) -> Hsla {
    match kind {
        Some(k) => {
            let hue = (0.6 + k as f32 * 0.618_034) % 1.0;
            let light = if k % 2 == 0 { 0.56 } else { 0.47 };
            hsla(hue, 0.72, light, 1.0)
        }
        None => hsla(0.0, 0.0, 0.45, 1.0),
    }
}

fn to_rect(b: Bounds<Pixels>) -> Rect {
    Rect::new(f32::from(b.origin.x), f32::from(b.origin.y), f32::from(b.size.width), f32::from(b.size.height))
}

fn to_bounds(r: Rect) -> Bounds<Pixels> {
    Bounds { origin: point(px(r.x), px(r.y)), size: size(px(r.w), px(r.h)) }
}

fn paint_cell(tree: &Tree, cell: &Cell, marked: &[NodeId], hovered: Option<Rect>, window: &mut Window, cx: &mut App) {
    let r = cell.rect;
    let is_hover = hovered == Some(r);
    let is_marked = matches!(cell.kind, CellKind::Node(id) if marked.contains(&id));
    let outline: Hsla = hsla(0.0, 0.0, 0.0, 0.55);

    let (label, is_dir) = match cell.kind {
        CellKind::Node(id) => {
            let n = tree.node(id);
            let suffix = if n.is_dir { "/" } else { "" };
            (format!("{}{} ({})", n.name, suffix, human_size(n.size)), n.is_dir)
        }
        CellKind::Others { count, size, .. } => (format!("ещё {count} ({})", human_size(size)), false),
    };

    if is_dir {
        // Папка: тонкая рамка и подпись, цвет дают файлы внутри.
        let header = hsla(0.0, 0.0, 1.0, if is_hover { 0.10 } else { 0.04 });
        window.paint_quad(quad(to_bounds(r), px(0.), header, px(1.), outline, Default::default()));
    } else {
        // Файл: «подушка» — градиент от светлого верхнего левого угла к тёмному нижнему правому.
        let base = match cell.kind {
            CellKind::Node(id) => type_color(tree.node(id).kind),
            CellKind::Others { .. } => hsla(0.0, 0.0, 0.35, 1.0),
        };
        let base = if is_hover { base.blend(hsla(0.0, 0.0, 1.0, 0.25)) } else { base };
        let light = Hsla { l: (base.l + 0.16).min(0.92), ..base };
        let dark = Hsla { l: (base.l - 0.20).max(0.05), ..base };
        let fill = linear_gradient(135., linear_color_stop(light, 0.0), linear_color_stop(dark, 1.0));
        window.paint_quad(quad(to_bounds(r), px(0.), fill, px(1.), outline, Default::default()));
    }
    if is_marked {
        window.paint_quad(quad(to_bounds(r), px(0.), hsla(0.0, 0.0, 0.0, 0.0), px(2.), rgb(0xffffff), Default::default()));
    }

    let show = if is_dir { r.h > 40.0 && r.w > 50.0 } else { r.w > 40.0 && r.h > 15.0 };
    if !show {
        return;
    }
    let max_chars = ((r.w - 8.0) / 6.0).max(0.0) as usize;
    let text = truncate(&label, max_chars);
    if text.is_empty() {
        return;
    }
    let color: Hsla = if is_dir { rgb(0xe6e9f5).into() } else { hsla(0.0, 0.0, 0.05, 0.9) };
    let run = TextRun { len: text.len(), font: window.text_style().font(), color, background_color: None, underline: None, strikethrough: None };
    let line = window.text_system().shape_line(text.into(), px(11.), &[run], None);
    let clip = to_bounds(r.inset(1.0));
    window.with_content_mask(Some(ContentMask { bounds: clip }), |window| {
        let _ = line.paint(point(px(r.x + 3.0), px(r.y)), px(HEADER_H), window, cx);
    });
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    if max_chars < 4 {
        return String::new();
    }
    let mut t: String = s.chars().take(max_chars - 1).collect();
    t.push('…');
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(group_digits(1234567), "1 234 567");
        assert_eq!(group_digits(999), "999");
        assert!(is_texty("text/x-rust") && is_texty("application/x-shellscript") && !is_texty("image/png"));
        assert_eq!(truncate("абвгдеж", 5), "абвг…");
    }
}
