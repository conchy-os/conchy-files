//! Окно conchy-files: дерево папок, типы файлов и treemap, связанные выделением.

use std::cell::RefCell;
use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use conchy_files::layout::{self, Cell, CellKind, HEADER_H};
use conchy_files::model::{NodeId, Tree, human_size};
use conchy_files::scan::{Progress, ScanOptions, scan};
use conchy_files::treemap::Rect;
use gpui::{
    App, Bounds, ClickEvent, Context, ContentMask, FocusHandle, Hsla, KeyBinding, MouseButton,
    MouseDownEvent, MouseMoveEvent, Pixels, ScrollStrategy, SharedString, Task, TextRun, Timer,
    UniformListScrollHandle, Window, actions, canvas, div, hsla, linear_color_stop, linear_gradient,
    point, prelude::*, px, quad, relative, rgb,
    size, uniform_list,
};

actions!(
    conchy_files,
    [SelectNext, SelectPrev, Expand, Collapse, ZoomIn, ZoomOut, OpenItem, Rescan, Quit]
);

const CTX: &str = "FilesView";

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("j", SelectNext, Some(CTX)),
        KeyBinding::new("down", SelectNext, Some(CTX)),
        KeyBinding::new("k", SelectPrev, Some(CTX)),
        KeyBinding::new("up", SelectPrev, Some(CTX)),
        KeyBinding::new("l", Expand, Some(CTX)),
        KeyBinding::new("right", Expand, Some(CTX)),
        KeyBinding::new("h", Collapse, Some(CTX)),
        KeyBinding::new("left", Collapse, Some(CTX)),
        KeyBinding::new("enter", ZoomIn, Some(CTX)),
        KeyBinding::new("backspace", ZoomOut, Some(CTX)),
        KeyBinding::new("o", OpenItem, Some(CTX)),
        KeyBinding::new("f5", Rescan, Some(CTX)),
        KeyBinding::new("q", Quit, Some(CTX)),
        KeyBinding::new("ctrl-q", Quit, Some(CTX)),
    ]);
}

// Палитра (Tokyo Night, как в стандартных конфигах Conchy).
const BG: u32 = 0x1a1b26;
const PANEL: u32 = 0x1f2335;
const BORDER: u32 = 0x292e42;
const TEXT: u32 = 0xc0caf5;
const DIM: u32 = 0x737aa2;
const ACCENT: u32 = 0x7aa2f7;
const SEL_BG: u32 = 0x2e3c64;

/// Свой цвет у каждого типа файлов, как в WizTree. Оттенки идут по золотому сечению,
/// поэтому соседние по размеру типы всегда заметно различаются.
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

const ROW_H: f32 = 22.0;

enum ScanState {
    Idle,
    Running { started: Instant, progress: Arc<Progress> },
    Done { took: Duration, errors: u64 },
    Failed(String),
}

pub struct FilesView {
    root_path: PathBuf,
    focus: FocusHandle,
    tree: Option<Arc<Tree>>,
    state: ScanState,
    _scan_task: Option<Task<()>>,

    /// Видимые строки дерева: (узел, глубина).
    rows: Vec<(NodeId, u16)>,
    expanded: HashSet<NodeId>,
    selected: NodeId,
    tree_scroll: UniformListScrollHandle,

    /// Корень, который сейчас показан на карте.
    view_root: NodeId,
    type_filter: Option<u16>,
    hovered: Option<Cell>,
    /// Последняя раскладка карты (заполняется при отрисовке, читается мышью).
    cells: Rc<RefCell<MapCache>>,
}

#[derive(Default)]
struct MapCache {
    key: Option<(NodeId, [i32; 4], usize)>,
    cells: Vec<Cell>,
}

impl FilesView {
    pub fn new(root_path: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        focus.focus(window);
        let mut this = Self {
            root_path,
            focus,
            tree: None,
            state: ScanState::Idle,
            _scan_task: None,
            rows: Vec::new(),
            expanded: HashSet::new(),
            selected: Tree::ROOT,
            tree_scroll: UniformListScrollHandle::new(),
            view_root: Tree::ROOT,
            type_filter: None,
            hovered: None,
            cells: Rc::default(),
        };
        this.start_scan(cx);
        this
    }

    // ---------- сканирование ----------

    fn start_scan(&mut self, cx: &mut Context<Self>) {
        if matches!(self.state, ScanState::Running { .. }) {
            return;
        }
        let progress = Arc::new(Progress::default());
        self.state = ScanState::Running { started: Instant::now(), progress: progress.clone() };
        let root = self.root_path.clone();
        let p = progress.clone();
        let job = cx.background_executor().spawn(async move {
            let t = Instant::now();
            let res = scan(&root, &ScanOptions::default(), &p).map(Tree::from_raw);
            (res, t.elapsed())
        });

        // Пока идёт обход, раз в 100 мс перерисовываем счётчики.
        cx.spawn(async move |this, cx| {
            loop {
                Timer::after(Duration::from_millis(100)).await;
                let running = this
                    .update(cx, |this, cx| {
                        cx.notify();
                        matches!(this.state, ScanState::Running { .. })
                    })
                    .unwrap_or(false);
                if !running {
                    break;
                }
            }
        })
        .detach();

        self._scan_task = Some(cx.spawn(async move |this, cx| {
            let (res, took) = job.await;
            let errors = progress.errors.load(Ordering::Relaxed);
            let _ = this.update(cx, |this, cx| {
                match res {
                    Ok(tree) => this.set_tree(tree, took, errors),
                    Err(e) => this.state = ScanState::Failed(e.to_string()),
                }
                cx.notify();
            });
        }));
    }

    fn set_tree(&mut self, tree: Tree, took: Duration, errors: u64) {
        // Сохраняем путь выделения, чтобы после пересканирования вернуться туда же.
        let old_path = self.tree.as_ref().map(|t| t.path(self.selected));
        self.tree = Some(Arc::new(tree));
        self.state = ScanState::Done { took, errors };
        self.expanded.clear();
        self.expanded.insert(Tree::ROOT);
        self.view_root = Tree::ROOT;
        self.selected = Tree::ROOT;
        self.type_filter = None;
        self.hovered = None;
        *self.cells.borrow_mut() = MapCache::default();
        if let Some(p) = old_path {
            if let Some(id) = self.find_by_path(&p) {
                self.reveal(id);
            }
        }
        self.rebuild_rows();
    }

    fn find_by_path(&self, path: &std::path::Path) -> Option<NodeId> {
        let tree = self.tree.as_ref()?;
        let rel = path.strip_prefix(&*tree.node(Tree::ROOT).name).ok()?;
        let mut cur = Tree::ROOT;
        for comp in rel.components() {
            let name = comp.as_os_str().to_string_lossy();
            cur = *tree.node(cur).children.iter().find(|&&c| *tree.node(c).name == *name)?;
        }
        Some(cur)
    }

    // ---------- дерево ----------

    fn rebuild_rows(&mut self) {
        self.rows.clear();
        let Some(tree) = self.tree.clone() else { return };
        let mut stack = vec![(Tree::ROOT, 0u16)];
        while let Some((id, depth)) = stack.pop() {
            self.rows.push((id, depth));
            if self.expanded.contains(&id) {
                for &c in tree.node(id).children.iter().rev() {
                    stack.push((c, depth + 1));
                }
            }
        }
    }

    fn selected_row(&self) -> Option<usize> {
        self.rows.iter().position(|(id, _)| *id == self.selected)
    }

    /// Выделяет узел, раскрывает его родителей и прокручивает дерево к нему.
    fn reveal(&mut self, id: NodeId) {
        let Some(tree) = self.tree.clone() else { return };
        for a in tree.ancestors(id) {
            if a != id {
                self.expanded.insert(a);
            }
        }
        self.selected = id;
        self.rebuild_rows();
        if let Some(ix) = self.selected_row() {
            self.tree_scroll.scroll_to_item(ix, ScrollStrategy::Center);
        }
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(ix) = self.selected_row() else { return };
        let next = (ix as isize + delta).clamp(0, self.rows.len() as isize - 1) as usize;
        self.selected = self.rows[next].0;
        self.tree_scroll.scroll_to_item(next, ScrollStrategy::Top);
        cx.notify();
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(1, cx);
    }

    fn select_prev(&mut self, _: &SelectPrev, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(-1, cx);
    }

    fn expand(&mut self, _: &Expand, _: &mut Window, cx: &mut Context<Self>) {
        let Some(tree) = self.tree.clone() else { return };
        let node = tree.node(self.selected);
        if !node.is_dir || node.children.is_empty() {
            return;
        }
        if self.expanded.insert(self.selected) {
            self.rebuild_rows();
        } else {
            // Уже раскрыт — переходим к первому ребёнку.
            self.move_selection(1, cx);
        }
        cx.notify();
    }

    fn collapse(&mut self, _: &Collapse, _: &mut Window, cx: &mut Context<Self>) {
        let Some(tree) = self.tree.clone() else { return };
        if self.expanded.remove(&self.selected) && self.selected != Tree::ROOT {
            self.rebuild_rows();
        } else if let Some(p) = tree.node(self.selected).parent {
            self.reveal(p);
        }
        cx.notify();
    }

    fn toggle_row(&mut self, id: NodeId, cx: &mut Context<Self>) {
        if !self.expanded.remove(&id) {
            self.expanded.insert(id);
        }
        self.selected = id;
        self.rebuild_rows();
        cx.notify();
    }

    // ---------- карта ----------

    fn zoom_to(&mut self, id: NodeId) {
        let Some(tree) = self.tree.as_ref() else { return };
        let node = tree.node(id);
        self.view_root = if node.is_dir { id } else { node.parent.unwrap_or(Tree::ROOT) };
        self.hovered = None;
    }

    fn zoom_in(&mut self, _: &ZoomIn, _: &mut Window, cx: &mut Context<Self>) {
        self.zoom_to(self.selected);
        cx.notify();
    }

    fn zoom_out(&mut self, _: &ZoomOut, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(p) = self.tree.as_ref().and_then(|t| t.node(self.view_root).parent) {
            let from = self.view_root;
            self.view_root = p;
            self.reveal(from);
            cx.notify();
        }
    }

    fn open_item(&mut self, _: &OpenItem, _: &mut Window, _: &mut Context<Self>) {
        if let Some(tree) = &self.tree {
            let _ = std::process::Command::new("xdg-open").arg(tree.path(self.selected)).spawn();
        }
    }

    fn rescan(&mut self, _: &Rescan, _: &mut Window, cx: &mut Context<Self>) {
        self.start_scan(cx);
        cx.notify();
    }

    fn quit(&mut self, _: &Quit, _: &mut Window, cx: &mut Context<Self>) {
        cx.quit();
    }

    fn cell_at(&self, pos: gpui::Point<Pixels>) -> Option<Cell> {
        let cache = self.cells.borrow();
        layout::hit(&cache.cells, f32::from(pos.x), f32::from(pos.y)).cloned()
    }

    fn on_map_mouse_down(&mut self, ev: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(cell) = self.cell_at(ev.position) else { return };
        let id = match cell.kind {
            CellKind::Node(id) => id,
            CellKind::Others { parent, .. } => parent,
        };
        if ev.click_count >= 2 {
            self.zoom_to(id);
        }
        self.reveal(id);
        cx.notify();
    }

    fn on_map_mouse_move(&mut self, ev: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        let cell = self.cell_at(ev.position);
        let same = match (&cell, &self.hovered) {
            (Some(a), Some(b)) => a.rect == b.rect,
            (None, None) => true,
            _ => false,
        };
        if !same {
            self.hovered = cell;
            cx.notify();
        }
    }

    // ---------- отрисовка ----------

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut crumbs = div().flex().items_center().gap_1().min_w_0().overflow_hidden();
        if let Some(tree) = &self.tree {
            for (i, id) in tree.ancestors(self.view_root).into_iter().enumerate() {
                if i > 0 {
                    crumbs = crumbs.child(div().text_color(rgb(DIM)).child("›"));
                }
                let name: SharedString = tree.node(id).name.to_string().into();
                let current = id == self.view_root;
                crumbs = crumbs.child(
                    div()
                        .id(("crumb", id as usize))
                        .px_1()
                        .rounded_sm()
                        .whitespace_nowrap()
                        .cursor_pointer()
                        .text_color(rgb(if current { TEXT } else { ACCENT }))
                        .hover(|s| s.bg(rgb(BORDER)))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.view_root = id;
                            this.reveal(id);
                            cx.notify();
                        }))
                        .child(name),
                );
            }
        } else {
            crumbs = crumbs.child(self.root_path.display().to_string());
        }

        let status: String = match &self.state {
            ScanState::Idle => String::new(),
            ScanState::Running { started, progress } => format!(
                "Сканирование… {} файлов · {} папок · {} · {:.1} с",
                progress.files.load(Ordering::Relaxed),
                progress.dirs.load(Ordering::Relaxed),
                human_size(progress.bytes.load(Ordering::Relaxed)),
                started.elapsed().as_secs_f32()
            ),
            ScanState::Done { took, errors } => {
                let tree = self.tree.as_ref().unwrap();
                let root = tree.node(Tree::ROOT);
                let mut s = format!(
                    "{} · на диске {} · {} файлов · за {:.2} с",
                    human_size(root.size),
                    human_size(root.alloc),
                    root.files,
                    took.as_secs_f32()
                );
                if *errors > 0 {
                    s.push_str(&format!(" · нет доступа: {errors}"));
                }
                s
            }
            ScanState::Failed(e) => format!("Ошибка: {e}"),
        };

        div()
            .flex()
            .items_center()
            .justify_between()
            .gap_4()
            .px_3()
            .h(px(36.))
            .flex_shrink_0()
            .bg(rgb(PANEL))
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(crumbs)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .flex_shrink_0()
                    .child(div().text_color(rgb(DIM)).child(status))
                    .child(
                        div()
                            .id("rescan")
                            .px_2()
                            .py_0p5()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(BORDER))
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(BORDER)))
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.start_scan(cx);
                                cx.notify();
                            }))
                            .child("Пересканировать (F5)"),
                    ),
            )
    }


    fn render_tree(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let head = div()
            .flex()
            .w_full()
            .h(px(ROW_H))
            .px_2()
            .items_center()
            .flex_shrink_0()
            .text_color(rgb(DIM))
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(div().flex_1().min_w_0().child("Папка"))
            .child(col(COL_PCT).child("% от папки"))
            .child(col(COL_SIZE).justify_end().child("Размер"))
            .child(col(COL_FILES).justify_end().child("Файлы"));

        let list = uniform_list(
            "tree",
            self.rows.len(),
            cx.processor(|this, range: std::ops::Range<usize>, _window, cx| {
                let Some(tree) = this.tree.clone() else { return Vec::new() };
                range
                    .map(|ix| {
                        let (id, depth) = this.rows[ix];
                        let n = tree.node(id);
                        let share = tree.share_of_parent(id);
                        let selected = id == this.selected;
                        let chevron = if !n.is_dir || n.children.is_empty() {
                            ""
                        } else if this.expanded.contains(&id) {
                            "▾"
                        } else {
                            "▸"
                        };
                        div()
                            .id(("row", ix))
                            .flex()
                            .w_full()
                            .items_center()
                            .h(px(ROW_H))
                            .px_2()
                            .cursor_pointer()
                            .when(selected, |d| d.bg(rgb(SEL_BG)))
                            .when(!selected, |d| d.hover(|s| s.bg(rgb(BORDER))))
                            .on_click(cx.listener(move |this, ev: &ClickEvent, _, cx| {
                                if ev.click_count() >= 2 {
                                    this.zoom_to(id);
                                    this.toggle_row(id, cx);
                                } else {
                                    this.selected = id;
                                    cx.notify();
                                }
                            }))
                            .child(
                                div()
                                    .flex()
                                    .flex_1()
                                    .min_w_0()
                                    .items_center()
                                    .overflow_hidden()
                                    .pl(px(depth as f32 * 14.))
                                    .child(
                                        div()
                                            .id(("chev", ix))
                                            .w(px(14.))
                                            .flex_shrink_0()
                                            .text_color(rgb(DIM))
                                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                                cx.stop_propagation();
                                                this.toggle_row(id, cx);
                                            }))
                                            .child(chevron),
                                    )
                                    .child(
                                        div()
                                            .size(px(9.))
                                            .mr(px(6.))
                                            .flex_shrink_0()
                                            .rounded_sm()
                                            .when(n.is_dir, |d| d.bg(rgb(0xe0af68)))
                                            .when(!n.is_dir, |d| d.bg(type_color(n.kind))),
                                    )
                                    .child(
                                        div()
                                            .min_w_0()
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .text_ellipsis()
                                            .when(!n.is_dir, |d| d.text_color(rgb(DIM)))
                                            .child(n.name.to_string()),
                                    ),
                            )
                            .child(col(COL_PCT).child(share_bar(share)))
                            .child(col(COL_SIZE).justify_end().child(human_size(n.size)))
                            .child(
                                col(COL_FILES)
                                    .justify_end()
                                    .text_color(rgb(DIM))
                                    .child(if n.is_dir { n.files.to_string() } else { String::new() }),
                            )
                    })
                    .collect()
            }),
        )
        .track_scroll(self.tree_scroll.clone())
        .flex_1()
        .w_full();

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .bg(rgb(BG))
            .child(head)
            .child(list)
    }

    fn render_types(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.tree.as_ref().map_or(0, |t| t.types.len());
        let head = div()
            .flex()
            .w_full()
            .h(px(ROW_H))
            .px_2()
            .items_center()
            .flex_shrink_0()
            .text_color(rgb(DIM))
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(div().flex_1().min_w_0().child("Тип файлов"))
            .child(col(COL_PCT_SHORT).justify_end().child("%"))
            .child(col(COL_SIZE).justify_end().child("Размер"))
            .child(col(COL_FILES).justify_end().child("Файлы"));

        let list = uniform_list(
            "types",
            count,
            cx.processor(|this, range: std::ops::Range<usize>, _window, cx| {
                let Some(tree) = this.tree.clone() else { return Vec::new() };
                let total = tree.node(Tree::ROOT).size.max(1) as f64;
                range
                    .map(|ix| {
                        let t = &tree.types[ix];
                        let k = ix as u16;
                        let active = this.type_filter == Some(k);
                        let label =
                            if t.ext.is_empty() { "без расширения".to_string() } else { format!(".{}", t.ext) };
                        div()
                            .id(("type", ix))
                            .flex()
                            .w_full()
                            .items_center()
                            .h(px(ROW_H))
                            .px_2()
                            .cursor_pointer()
                            .when(active, |d| d.bg(rgb(SEL_BG)))
                            .when(!active, |d| d.hover(|s| s.bg(rgb(BORDER))))
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.type_filter = if this.type_filter == Some(k) { None } else { Some(k) };
                                cx.notify();
                            }))
                            .child(
                                div()
                                    .flex()
                                    .flex_1()
                                    .min_w_0()
                                    .items_center()
                                    .gap_2()
                                    .overflow_hidden()
                                    .child(div().size(px(10.)).flex_shrink_0().rounded_sm().bg(type_color(Some(k))))
                                    .child(div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().child(label)),
                            )
                            .child(
                                col(COL_PCT_SHORT)
                                    .justify_end()
                                    .text_color(rgb(DIM))
                                    .child(format!("{:.1}%", t.size as f64 / total * 100.0)),
                            )
                            .child(col(COL_SIZE).justify_end().child(human_size(t.size)))
                            .child(col(COL_FILES).justify_end().text_color(rgb(DIM)).child(t.files.to_string()))
                    })
                    .collect()
            }),
        )
        .flex_1()
        .w_full();

        div()
            .flex()
            .flex_col()
            .w(relative(0.36))
            .min_w(px(280.))
            .flex_shrink_0()
            .bg(rgb(BG))
            .border_l_1()
            .border_color(rgb(BORDER))
            .child(head)
            .child(list)
    }

    fn render_map(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tree = self.tree.clone();
        let cache = self.cells.clone();
        let view_root = self.view_root;
        let selected = self.selected;
        let filter = self.type_filter;
        let hovered_rect = self.hovered.as_ref().map(|c| c.rect);

        let map = canvas(
            move |_, _, _| {},
            move |bounds, _, window, cx| {
                window.paint_quad(quad(bounds, px(0.), rgb(MAP_BG), px(0.), rgb(MAP_BG), Default::default()));
                let Some(tree) = tree else { return };
                let b = to_rect(bounds);
                let key = (
                    view_root,
                    [b.x as i32, b.y as i32, b.w as i32, b.h as i32],
                    Arc::as_ptr(&tree) as usize,
                );
                let mut cache = cache.borrow_mut();
                if cache.key != Some(key) {
                    cache.cells = layout::layout(&tree, view_root, b.inset(1.0));
                    cache.key = Some(key);
                }
                window.with_content_mask(Some(ContentMask { bounds }), |window| {
                    for cell in &cache.cells {
                        paint_cell(&tree, cell, selected, filter, hovered_rect, window, cx);
                    }
                });
            },
        )
        .size_full();

        div()
            .id("map")
            .flex_1()
            .min_h_0()
            .border_t_1()
            .border_color(rgb(BORDER))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_map_mouse_down))
            .on_mouse_move(cx.listener(Self::on_map_mouse_move))
            .child(map)
    }

    /// Одна строка состояния снизу: что под курсором или что выделено.
    fn render_status(&self) -> impl IntoElement {
        let text: String = match &self.tree {
            None => String::new(),
            Some(tree) => {
                let describe = |id: NodeId| {
                    let n = tree.node(id);
                    let mut s = format!(
                        "{}  ·  {}  ·  {:.1}% от папки",
                        tree.path(id).display(),
                        human_size(n.size),
                        tree.share_of_parent(id) * 100.0
                    );
                    if n.is_dir {
                        s.push_str(&format!("  ·  {} файлов", n.files));
                    }
                    s
                };
                match self.hovered.as_ref().map(|c| &c.kind) {
                    Some(CellKind::Node(id)) => describe(*id),
                    Some(CellKind::Others { parent, count, size }) => format!(
                        "{}  ·  ещё {} мелких элементов  ·  {}",
                        tree.path(*parent).display(),
                        count,
                        human_size(*size)
                    ),
                    None => describe(self.selected),
                }
            }
        };
        div()
            .h(px(22.))
            .px_3()
            .flex()
            .items_center()
            .flex_shrink_0()
            .bg(rgb(PANEL))
            .border_t_1()
            .border_color(rgb(BORDER))
            .text_color(rgb(DIM))
            .overflow_hidden()
            .whitespace_nowrap()
            .child(text)
    }
}

const COL_PCT: f32 = 120.0;
const COL_PCT_SHORT: f32 = 56.0;
const COL_SIZE: f32 = 84.0;
const COL_FILES: f32 = 72.0;
const MAP_BG: u32 = 0x101018;

/// Колонка фиксированной ширины: заголовок и строки используют одну и ту же функцию,
/// поэтому значения всегда стоят ровно под своими заголовками.
fn col(w: f32) -> gpui::Div {
    div().w(px(w)).flex_shrink_0().flex().items_center().pl_2().whitespace_nowrap()
}

/// Полоска «% от папки» с числом поверх, как в WizTree.
fn share_bar(share: f64) -> impl IntoElement {
    let w = COL_PCT - 12.0;
    div()
        .relative()
        .w(px(w))
        .h(px(15.))
        .rounded_sm()
        .bg(rgb(BORDER))
        .child(
            div()
                .absolute()
                .top_0()
                .left_0()
                .h_full()
                .rounded_sm()
                .bg(rgb(0x3d59a1))
                .w(px((share.clamp(0.0, 1.0) * w as f64) as f32)),
        )
        .child(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(format!("{:.1}%", share * 100.0)),
        )
}

fn to_rect(b: Bounds<Pixels>) -> Rect {
    Rect::new(
        f32::from(b.origin.x),
        f32::from(b.origin.y),
        f32::from(b.size.width),
        f32::from(b.size.height),
    )
}

fn to_bounds(r: Rect) -> Bounds<Pixels> {
    Bounds { origin: point(px(r.x), px(r.y)), size: size(px(r.w), px(r.h)) }
}

fn paint_cell(
    tree: &Tree,
    cell: &Cell,
    selected: NodeId,
    filter: Option<u16>,
    hovered: Option<Rect>,
    window: &mut Window,
    cx: &mut App,
) {
    let r = cell.rect;
    let is_hover = hovered == Some(r);
    let is_selected = matches!(cell.kind, CellKind::Node(id) if id == selected);
    let outline: Hsla = hsla(0.0, 0.0, 0.0, 0.55);

    let (label, is_dir) = match cell.kind {
        CellKind::Node(id) => {
            let n = tree.node(id);
            let suffix = if n.is_dir { "\\" } else { "" };
            (format!("{}{} ({})", n.name, suffix, human_size(n.size)), n.is_dir)
        }
        CellKind::Others { count, size, .. } => (format!("ещё {count} ({})", human_size(size)), false),
    };

    if is_dir {
        // Папка: только тонкая рамка и подпись, цвет дают файлы внутри.
        let header = hsla(0.0, 0.0, 1.0, if is_hover { 0.10 } else { 0.04 });
        window.paint_quad(quad(to_bounds(r), px(0.), header, px(1.), outline, Default::default()));
    } else {
        // Файл: «подушка» — градиент от светлого верхнего левого угла к тёмному нижнему правому.
        let base = match cell.kind {
            CellKind::Node(id) => {
                let n = tree.node(id);
                let c = type_color(n.kind);
                if filter.is_some() && filter != n.kind { c.opacity(0.15) } else { c }
            }
            CellKind::Others { .. } => hsla(0.0, 0.0, 0.35, 1.0),
        };
        let base = if is_hover { base.blend(hsla(0.0, 0.0, 1.0, 0.25)) } else { base };
        let light = Hsla { l: (base.l + 0.16).min(0.92), ..base };
        let dark = Hsla { l: (base.l - 0.20).max(0.05), ..base };
        let fill = linear_gradient(135., linear_color_stop(light, 0.0), linear_color_stop(dark, 1.0));
        window.paint_quad(quad(to_bounds(r), px(0.), fill, px(1.), outline, Default::default()));
    }
    if is_selected {
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
    let run = TextRun {
        len: text.len(),
        font: window.text_style().font(),
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
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

impl Render for FilesView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context(CTX)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_prev))
            .on_action(cx.listener(Self::expand))
            .on_action(cx.listener(Self::collapse))
            .on_action(cx.listener(Self::zoom_in))
            .on_action(cx.listener(Self::zoom_out))
            .on_action(cx.listener(Self::open_item))
            .on_action(cx.listener(Self::rescan))
            .on_action(cx.listener(Self::quit))
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .text_xs()
            .child(self.render_header(cx))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_tree(cx))
                    .child(self.render_types(cx)),
            )
            .child(self.render_map(cx))
            .child(self.render_status())
    }
}
