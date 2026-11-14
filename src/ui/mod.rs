//! Окно conchy-files в стиле Thunar: панель пути, боковая панель «Места»
//! и «Устройства», список файлов и строка состояния.

mod actions;
mod dialogs;
mod field;
mod panel;
mod view;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use conchy_files::dir::{self, Entry, Sort, SortKey, TrashInfo};
use conchy_files::model::human_size;
use conchy_files::places::{self, Mount, Place, Target};
use conchy_files::settings::{FilesConfig, FolderSize, Settings, SortBy};
use conchy_files::{icons, mime, trash, watch};
use futures::StreamExt;
use gpui::{
    App, Context, FocusHandle, KeyBinding, KeyDownEvent, Modifiers, MouseButton, MouseDownEvent, NavigationDirection,
    ScrollStrategy, Task, Timer, UniformListScrollHandle, Window, actions, div, prelude::*, px, rgb,
};

use actions::{Clip, Job};
use dialogs::{Dialog, Menu};
use field::TextField;

actions!(
    conchy_files,
    [
        SelectNext, SelectPrev, ExtendNext, ExtendPrev, SelectFirst, SelectLast, PageDown, PageUp, OpenSelected,
        GoBack, GoForward, GoUp, GoHome, EditLocation, ToggleHidden, Reload, CloseWindow, SelectAll, Escape,
        CutFiles, CopyFiles, PasteFiles, RenameItem, NewFolder, TrashFiles, DeleteFiles, Undo, TogglePanel,
        ShowPanel
    ]
);

const CTX: &str = "FilesView";
/// Пока вводится текст (диалог или путь), привязки окна не срабатывают.
const CTX_TEXT: &str = "FilesText";

/// Клавиши Thunar 4.20.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("down", SelectNext, Some(CTX)),
        KeyBinding::new("up", SelectPrev, Some(CTX)),
        KeyBinding::new("shift-down", ExtendNext, Some(CTX)),
        KeyBinding::new("shift-up", ExtendPrev, Some(CTX)),
        KeyBinding::new("home", SelectFirst, Some(CTX)),
        KeyBinding::new("end", SelectLast, Some(CTX)),
        KeyBinding::new("pagedown", PageDown, Some(CTX)),
        KeyBinding::new("pageup", PageUp, Some(CTX)),
        KeyBinding::new("enter", OpenSelected, Some(CTX)),
        KeyBinding::new("backspace", GoBack, Some(CTX)),
        KeyBinding::new("alt-left", GoBack, Some(CTX)),
        KeyBinding::new("alt-right", GoForward, Some(CTX)),
        KeyBinding::new("alt-up", GoUp, Some(CTX)),
        KeyBinding::new("alt-home", GoHome, Some(CTX)),
        KeyBinding::new("ctrl-l", EditLocation, Some(CTX)),
        KeyBinding::new("alt-d", EditLocation, Some(CTX)),
        KeyBinding::new("ctrl-h", ToggleHidden, Some(CTX)),
        KeyBinding::new("f5", Reload, Some(CTX)),
        KeyBinding::new("ctrl-r", Reload, Some(CTX)),
        KeyBinding::new("ctrl-q", CloseWindow, Some(CTX)),
        KeyBinding::new("ctrl-w", CloseWindow, Some(CTX)),
        KeyBinding::new("ctrl-a", SelectAll, Some(CTX)),
        KeyBinding::new("escape", Escape, Some(CTX)),
        KeyBinding::new("ctrl-x", CutFiles, Some(CTX)),
        KeyBinding::new("ctrl-c", CopyFiles, Some(CTX)),
        KeyBinding::new("ctrl-insert", CopyFiles, Some(CTX)),
        KeyBinding::new("ctrl-v", PasteFiles, Some(CTX)),
        KeyBinding::new("shift-insert", PasteFiles, Some(CTX)),
        KeyBinding::new("f2", RenameItem, Some(CTX)),
        KeyBinding::new("ctrl-shift-n", NewFolder, Some(CTX)),
        KeyBinding::new("delete", TrashFiles, Some(CTX)),
        KeyBinding::new("shift-delete", DeleteFiles, Some(CTX)),
        KeyBinding::new("ctrl-z", Undo, Some(CTX)),
        // Нижняя панель: анализ места и свойства (в Thunar Alt+Enter — «Свойства»).
        KeyBinding::new("ctrl-i", TogglePanel, Some(CTX)),
        KeyBinding::new("alt-enter", ShowPanel, Some(CTX)),
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
const DANGER: u32 = 0xf7768e;

/// Строка списка: запись и заранее подготовленные подписи (готовятся в фоне).
pub(crate) struct Row {
    pub entry: Entry,
    pub icon: Option<PathBuf>,
    pub kind: String,
    pub size: String,
    pub date: String,
}

impl AsRef<Entry> for Row {
    fn as_ref(&self) -> &Entry {
        &self.entry
    }
}

pub struct FilesWindow {
    focus: FocusHandle,
    location: Target,
    back: Vec<Target>,
    forward: Vec<Target>,

    /// Всё содержимое папки (отсортировано) и индексы видимых строк (без скрытых).
    rows: Vec<Row>,
    visible: Vec<usize>,
    sort: Sort,
    show_hidden: bool,
    loading: bool,
    load_error: Option<String>,
    load_gen: u64,
    _load_task: Option<Task<()>>,
    /// (всего, свободно) на разделе текущей папки.
    space: Option<(u64, u64)>,

    /// Выделение по путям: переживает перечитывание папки.
    selected: HashSet<PathBuf>,
    cursor: Option<PathBuf>,
    anchor: Option<PathBuf>,
    list_scroll: UniformListScrollHandle,
    typeahead: (String, Instant),

    location_edit: Option<TextField>,
    places: Vec<Place>,
    mounts: Vec<Mount>,
    /// Особые иконки XDG-папок (`folder-download` и т. п.).
    special_icons: HashMap<PathBuf, String>,
    _watch: Option<watch::Watch>,
    title: String,

    /// Канал `files` единого конфига Conchy (`~/.config/conchy/files.kdl`).
    settings: Arc<Settings>,
    config: FilesConfig,
    _config_watch: Option<watch::Watch>,
    panel: panel::PanelState,

    clipboard: Option<Clip>,
    undo: Vec<conchy_files::ops::Record>,
    job: Option<Job>,
    dialog: Option<Dialog>,
    menu: Option<Menu>,
    notice: Option<(String, Instant)>,
}

impl FilesWindow {
    pub fn new(
        start: PathBuf,
        settings: Arc<Settings>,
        config: FilesConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        focus.focus(window);
        icons::set_theme(config.icon_theme.as_deref());
        let special_icons = places::user_dirs()
            .into_iter()
            .map(|(_, p)| {
                let icon = places::folder_icon(&p);
                (p, icon)
            })
            .chain([(places::home(), "user-home".to_string())])
            .collect();
        let (location, select) = if start.is_dir() {
            (Target::Dir(start), Vec::new())
        } else {
            // Передали файл — открываем его папку и выделяем его, как `thunar файл`.
            let parent = start.parent().map(Path::to_path_buf).unwrap_or_else(places::home);
            (Target::Dir(parent), vec![start])
        };
        let mut this = Self {
            focus,
            location: location.clone(),
            back: Vec::new(),
            forward: Vec::new(),
            rows: Vec::new(),
            visible: Vec::new(),
            sort: sort_from(&config),
            show_hidden: config.show_hidden,
            loading: false,
            load_error: None,
            load_gen: 0,
            _load_task: None,
            space: None,
            selected: HashSet::new(),
            cursor: None,
            anchor: None,
            list_scroll: UniformListScrollHandle::new(),
            typeahead: (String::new(), Instant::now()),
            location_edit: None,
            places: places::places(),
            mounts: places::mounts(),
            special_icons,
            _watch: None,
            title: String::new(),
            settings,
            config,
            _config_watch: None,
            panel: Default::default(),
            clipboard: None,
            undo: Vec::new(),
            job: None,
            dialog: None,
            menu: None,
            notice: None,
        };
        this.configure_cache(cx);
        // Окно закрывают — дописываем на диск то, что ещё не записано.
        cx.on_release(|this, _| this.flush_cache_now()).detach();
        this.set_location(location, select, cx);
        this.poll_mounts(cx);
        this.watch_config(cx);
        this
    }

    // ---------- настройки (files.kdl) ----------

    /// Следит за `~/.config/conchy`: правки руками, `conchy-config set files …` и записи
    /// демона применяются сразу, без перезапуска.
    fn watch_config(&mut self, cx: &mut Context<Self>) {
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<()>();
        let Ok(w) = self.settings.watch(move || {
            let _ = tx.unbounded_send(());
        }) else {
            return;
        };
        self._config_watch = Some(w);
        cx.spawn(async move |this, cx| {
            while rx.next().await.is_some() {
                if this.update(cx, |this, cx| this.reload_config(cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    fn reload_config(&mut self, cx: &mut Context<Self>) {
        match self.settings.load() {
            Ok(c) => self.apply_config(c, cx),
            // Ошибка в файле: оставляем прежние настройки и говорим, где искать.
            Err(e) => {
                let first = e.lines().next().unwrap_or_default().to_string();
                self.notice(format!("files.kdl не применён: {first}"), cx);
            }
        }
    }

    fn apply_config(&mut self, c: FilesConfig, cx: &mut Context<Self>) {
        if c == self.config {
            return;
        }
        let old = std::mem::replace(&mut self.config, c);
        let mut reload = old.folder_size != self.config.folder_size;
        let cache_cfg = |c: &FilesConfig| (c.cache_enabled, c.cache_max_mb, c.cache_max_age_days);
        if cache_cfg(&old) != cache_cfg(&self.config) {
            self.configure_cache(cx);
        }
        if old.icon_theme != self.config.icon_theme && icons::set_theme(self.config.icon_theme.as_deref()) {
            reload = true;
        }
        let sort = sort_from(&self.config);
        if sort != self.sort {
            self.sort = sort;
            dir::sort(&mut self.rows, sort);
        }
        if self.config.show_hidden != self.show_hidden {
            self.show_hidden = self.config.show_hidden;
            let hidden: HashSet<PathBuf> =
                self.rows.iter().filter(|r| r.entry.is_hidden()).map(|r| r.entry.path.clone()).collect();
            if !self.show_hidden {
                self.selected.retain(|p| !hidden.contains(p));
            }
        }
        self.rebuild_visible();
        if reload {
            self.reload(cx);
        }
        self.sync_panel(cx);
        self.notice("Настройки из files.kdl применены", cx);
        cx.notify();
    }

    /// Запоминает значения в `files.kdl` (в фоне: запись идёт через D-Bus или файл).
    fn save_settings(&mut self, values: Vec<(&'static str, String)>, cx: &mut Context<Self>) {
        let settings = self.settings.clone();
        cx.background_executor()
            .spawn(async move {
                for (k, v) in values {
                    let _ = settings.set(k, &v);
                }
            })
            .detach();
    }

    // ---------- загрузка ----------

    /// Перечитывает текущее место в фоне. `select` — что выделить после загрузки.
    fn load(&mut self, select: Vec<PathBuf>, cx: &mut Context<Self>) {
        self.load_gen += 1;
        let generation = self.load_gen;
        self.loading = true;
        let target = self.location.clone();
        let tops: Vec<PathBuf> = self.mounts.iter().map(|m| m.mount_point.clone()).collect();
        let sort = self.sort;
        let special = self.special_icons.clone();
        let count_items = self.config.folder_size == FolderSize::Items;
        let job = cx
            .background_executor()
            .spawn(async move { read_location(&target, &tops, sort, count_items, &special) });
        self._load_task = Some(cx.spawn(async move |this, cx| {
            let res = job.await;
            let _ = this.update(cx, |this, cx| {
                if this.load_gen == generation {
                    this.apply_load(res, select);
                    this.apply_deep_sizes();
                    this.refine_types(cx);
                    cx.notify();
                }
            });
        }));
    }

    /// Уточняет в фоне типы файлов без говорящего имени (по первым байтам):
    /// список уже показан, а `/usr/bin` на холодном диске читался бы секунды.
    fn refine_types(&mut self, cx: &mut Context<Self>) {
        let generation = self.load_gen;
        let todo: Vec<(PathBuf, String)> = self
            .rows
            .iter()
            .filter(|r| !r.entry.is_dir && !r.entry.broken && mime::needs_sniff(&r.entry.mime))
            .map(|r| (r.entry.path.clone(), r.entry.mime.clone()))
            .collect();
        if todo.is_empty() {
            return;
        }
        let job = cx.background_executor().spawn(async move {
            todo.into_iter()
                .filter_map(|(p, old)| {
                    let m = mime::sniff(&p);
                    (m != old).then(|| (p, m.to_string()))
                })
                .collect::<Vec<_>>()
        });
        cx.spawn(async move |this, cx| {
            let found = job.await;
            let _ = this.update(cx, |this, cx| {
                if this.load_gen != generation || found.is_empty() {
                    return;
                }
                let found: HashMap<PathBuf, String> = found.into_iter().collect();
                for r in &mut this.rows {
                    if let Some(m) = found.get(&r.entry.path) {
                        r.entry.mime = m.clone();
                        let (icon, kind) = icon_and_kind(&r.entry, &this.special_icons);
                        r.icon = icon;
                        r.kind = kind;
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        self.load(Vec::new(), cx);
    }

    fn apply_load(&mut self, res: Result<Vec<Row>, String>, select: Vec<PathBuf>) {
        self.loading = false;
        match res {
            Ok(rows) => {
                self.rows = rows;
                self.load_error = None;
            }
            Err(e) => {
                self.rows.clear();
                self.load_error = Some(e);
            }
        }
        self.space = match &self.location {
            Target::Dir(p) => places::statvfs(p),
            Target::Trash => None,
        };
        self.rebuild_visible();
        let present: HashSet<&Path> = self.visible.iter().map(|&i| self.rows[i].entry.path.as_path()).collect();
        if !select.is_empty() {
            self.selected = select.iter().filter(|p| present.contains(p.as_path())).cloned().collect();
            let first = self.visible.iter().map(|&i| &self.rows[i].entry.path).find(|p| self.selected.contains(*p)).cloned();
            if let Some(first) = first {
                self.cursor = Some(first);
                self.anchor = self.cursor.clone();
                self.scroll_to_cursor();
            }
        } else {
            self.selected.retain(|p| present.contains(p.as_path()));
            if self.cursor.as_ref().is_some_and(|c| !present.contains(c.as_path())) {
                self.cursor = None;
            }
        }
    }

    fn rebuild_visible(&mut self) {
        let hidden = self.show_hidden;
        self.visible = (0..self.rows.len()).filter(|&i| hidden || !self.rows[i].entry.is_hidden()).collect();
    }

    pub(crate) fn resort(&mut self, key: SortKey, cx: &mut Context<Self>) {
        if self.sort.key == key {
            self.sort.ascending = !self.sort.ascending;
        } else {
            self.sort.key = key;
            self.sort.ascending = true;
        }
        dir::sort(&mut self.rows, self.sort);
        self.rebuild_visible();
        self.scroll_to_cursor();
        // Как Thunar: последняя сортировка запоминается.
        self.config.sort_by = match key {
            SortKey::Name => SortBy::Name,
            SortKey::Size => SortBy::Size,
            SortKey::Type => SortBy::Type,
            SortKey::Modified => SortBy::Modified,
        };
        self.config.sort_ascending = self.sort.ascending;
        let order = if self.sort.ascending { "ascending" } else { "descending" };
        self.save_settings(vec![("view.sort-by", self.config.sort_by.as_str().into()), ("view.sort-order", order.into())], cx);
        cx.notify();
    }

    /// Раз в 2 секунды обновляет «Устройства» (флешку вставили или извлекли).
    fn poll_mounts(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                Timer::after(Duration::from_secs(2)).await;
                let mounts = cx.background_executor().spawn(async { places::mounts() }).await;
                let alive = this
                    .update(cx, |this, cx| {
                        // Сравниваем с точностью до мегабайта, чтобы не перерисовывать из-за логов.
                        let key = |m: &[Mount]| m.iter().map(|x| (x.device.clone(), x.free >> 20)).collect::<Vec<_>>();
                        if key(&this.mounts) != key(&mounts) {
                            this.mounts = mounts;
                            cx.notify();
                        }
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        })
        .detach();
    }

    // ---------- навигация ----------

    pub(crate) fn navigate(&mut self, target: Target, cx: &mut Context<Self>) {
        if target == self.location {
            self.reload(cx);
            return;
        }
        self.back.push(self.location.clone());
        self.forward.clear();
        self.set_location(target, Vec::new(), cx);
    }

    fn set_location(&mut self, target: Target, mut select: Vec<PathBuf>, cx: &mut Context<Self>) {
        // Поднялись выше — выделяем папку, из которой пришли (как Thunar).
        if select.is_empty() {
            if let (Target::Dir(old), Target::Dir(new)) = (&self.location, &target) {
                if let Some(child) = old.ancestors().find(|a| a.parent() == Some(new.as_path())) {
                    select.push(child.to_path_buf());
                }
            }
        }
        self.location = target;
        self.rows.clear();
        self.visible.clear();
        self.selected.clear();
        self.cursor = None;
        self.anchor = None;
        self.location_edit = None;
        self.menu = None;
        self.load_error = None;
        self.list_scroll.scroll_to_item(0, ScrollStrategy::Top);
        self.start_watch(cx);
        self.load(select, cx);
        cx.notify();
    }

    fn start_watch(&mut self, cx: &mut Context<Self>) {
        self._watch = None;
        let dirs: Vec<PathBuf> = match &self.location {
            Target::Dir(p) => vec![p.clone()],
            Target::Trash => {
                let home = std::env::var_os("XDG_DATA_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| places::home().join(".local/share"))
                    .join("Trash");
                vec![home.join("files"), home.join("info")]
            }
        };
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<()>();
        let refs: Vec<&Path> = dirs.iter().map(PathBuf::as_path).collect();
        let Ok(w) = watch::watch(&refs, move || {
            let _ = tx.unbounded_send(());
        }) else {
            return;
        };
        self._watch = Some(w);
        cx.spawn(async move |this, cx| {
            while rx.next().await.is_some() {
                if this.update(cx, |this, cx| this.reload(cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    pub(crate) fn go_back(&mut self, _: &GoBack, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(prev) = self.back.pop() {
            self.forward.push(self.location.clone());
            self.set_location(prev, Vec::new(), cx);
        }
    }

    pub(crate) fn go_forward(&mut self, _: &GoForward, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(next) = self.forward.pop() {
            self.back.push(self.location.clone());
            self.set_location(next, Vec::new(), cx);
        }
    }

    pub(crate) fn go_up(&mut self, _: &GoUp, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(parent) = self.current_dir().and_then(Path::parent) {
            let parent = parent.to_path_buf();
            self.navigate(Target::Dir(parent), cx);
        }
    }

    pub(crate) fn go_home(&mut self, _: &GoHome, _: &mut Window, cx: &mut Context<Self>) {
        self.navigate(Target::Dir(places::home()), cx);
    }

    pub(crate) fn current_dir(&self) -> Option<&Path> {
        match &self.location {
            Target::Dir(p) => Some(p),
            Target::Trash => None,
        }
    }

    pub(crate) fn in_trash(&self) -> bool {
        self.location == Target::Trash
    }

    /// Открывает строку: папку — в этом окне, файл — программой по умолчанию.
    pub(crate) fn open_row(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.in_trash() {
            return self.notice("Чтобы открыть файл из корзины, сначала восстановите его", cx);
        }
        let e = &self.rows[ix].entry;
        if e.is_dir {
            let p = e.path.clone();
            self.navigate(Target::Dir(p), cx);
        } else if e.broken {
            let msg = format!("Ссылка «{}» никуда не ведёт", e.name);
            self.notice(msg, cx);
        } else {
            let _ = std::process::Command::new("xdg-open").arg(&e.path).spawn();
        }
    }

    fn open_selected(&mut self, _: &OpenSelected, _: &mut Window, cx: &mut Context<Self>) {
        let rows = self.selected_rows();
        // Среди выделенного есть папка — переходим в неё; иначе открываем все файлы.
        if let Some(&dir_ix) = rows.iter().find(|&&i| self.rows[i].entry.is_dir) {
            return self.open_row(dir_ix, cx);
        }
        for ix in rows {
            self.open_row(ix, cx);
        }
    }

    // ---------- выделение ----------

    /// Индексы `rows` выделенных строк в порядке показа.
    pub(crate) fn selected_rows(&self) -> Vec<usize> {
        self.visible.iter().copied().filter(|&i| self.selected.contains(&self.rows[i].entry.path)).collect()
    }

    fn cursor_pos(&self) -> Option<usize> {
        let c = self.cursor.as_ref()?;
        self.visible.iter().position(|&i| self.rows[i].entry.path == *c)
    }

    fn scroll_to_cursor(&mut self) {
        if let Some(pos) = self.cursor_pos() {
            self.list_scroll.scroll_to_item(pos, ScrollStrategy::Center);
        }
    }

    /// Нажатие на строку `pos` (позиция в видимом списке) с учётом Ctrl и Shift.
    pub(crate) fn click_row(&mut self, pos: usize, m: Modifiers) {
        let path = self.rows[self.visible[pos]].entry.path.clone();
        if m.shift {
            self.select_range(pos);
        } else if m.control {
            if !self.selected.remove(&path) {
                self.selected.insert(path.clone());
            }
            self.anchor = Some(path.clone());
        } else {
            self.selected.clear();
            self.selected.insert(path.clone());
            self.anchor = Some(path.clone());
        }
        self.cursor = Some(path);
        self.menu = None;
        self.location_edit = None;
    }

    fn select_range(&mut self, to: usize) {
        let from = self
            .anchor
            .as_ref()
            .and_then(|a| self.visible.iter().position(|&i| self.rows[i].entry.path == *a))
            .unwrap_or(to);
        let (a, b) = (from.min(to), from.max(to));
        self.selected = self.visible[a..=b].iter().map(|&i| self.rows[i].entry.path.clone()).collect();
    }

    fn move_cursor(&mut self, delta: isize, extend: bool, cx: &mut Context<Self>) {
        if self.visible.is_empty() {
            return;
        }
        let last = self.visible.len() as isize - 1;
        let pos = match self.cursor_pos() {
            Some(p) => (p as isize).saturating_add(delta).clamp(0, last) as usize,
            None if delta < 0 => last as usize,
            None => 0,
        };
        let path = self.rows[self.visible[pos]].entry.path.clone();
        if extend {
            if self.anchor.is_none() {
                self.anchor = Some(self.cursor.clone().unwrap_or_else(|| path.clone()));
            }
            self.select_range(pos);
        } else {
            self.selected.clear();
            self.selected.insert(path.clone());
            self.anchor = Some(path.clone());
        }
        self.cursor = Some(path);
        self.list_scroll.scroll_to_item(pos, ScrollStrategy::Top);
        cx.notify();
    }

    /// Клавиши без привязок: ввод текста (диалог, путь) или поиск по первым буквам,
    /// как в списке Thunar (набранное сбрасывается через секунду).
    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.dialog.is_some() {
            return self.on_dialog_key(ev, window, cx);
        }
        if self.location_edit.is_some() {
            return self.on_location_key(ev, cx);
        }
        let ks = &ev.keystroke;
        let m = &ks.modifiers;
        let Some(ch) = ks.key_char.clone().filter(|c| !m.control && !m.alt && !m.platform && !c.chars().any(char::is_control))
        else {
            return;
        };
        if self.typeahead.1.elapsed() > Duration::from_secs(1) {
            self.typeahead.0.clear();
        }
        if ch.trim().is_empty() && self.typeahead.0.is_empty() {
            return;
        }
        self.typeahead.0.push_str(&ch);
        self.typeahead.1 = Instant::now();
        let prefix = self.typeahead.0.to_lowercase();
        if let Some(pos) = self.visible.iter().position(|&i| self.rows[i].entry.name.to_lowercase().starts_with(&prefix)) {
            self.click_row(pos, Modifiers::default());
            self.list_scroll.scroll_to_item(pos, ScrollStrategy::Top);
        }
        cx.stop_propagation();
        cx.notify();
    }

    // ---------- строка пути ----------

    fn edit_location(&mut self, _: &EditLocation, _: &mut Window, cx: &mut Context<Self>) {
        self.start_location_edit(cx);
    }

    pub(crate) fn start_location_edit(&mut self, cx: &mut Context<Self>) {
        let text = match &self.location {
            Target::Dir(p) => p.display().to_string(),
            Target::Trash => "trash:///".into(),
        };
        let len = text.len();
        self.location_edit = Some(TextField::new(text, 0..len));
        cx.notify();
    }

    fn on_location_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        cx.stop_propagation();
        match ks.key.as_str() {
            "escape" => self.location_edit = None,
            "enter" => {
                let text = self.location_edit.take().map(|f| f.text).unwrap_or_default();
                self.go_to_typed(text.trim(), cx);
            }
            "tab" => {
                if let Some(f) = &mut self.location_edit {
                    if let Some(done) = complete_path(&f.text) {
                        let len = done.len();
                        *f = TextField::new(done, len..len);
                    }
                }
            }
            _ => {
                if let Some(f) = &mut self.location_edit {
                    let m = &ks.modifiers;
                    if (ks.key == "v" && m.control) || (ks.key == "insert" && m.shift) {
                        if let Some(t) = cx.read_from_clipboard().and_then(|c| c.text()) {
                            f.insert(t.lines().next().unwrap_or(""));
                        }
                    } else {
                        f.key(ks);
                    }
                }
            }
        }
        cx.notify();
    }

    /// Переход по введённому адресу: папка, файл (открыть его папку и выделить), `~`, `trash:///`.
    fn go_to_typed(&mut self, text: &str, cx: &mut Context<Self>) {
        if text.starts_with("trash:") {
            return self.navigate(Target::Trash, cx);
        }
        let text = text.strip_prefix("file://").unwrap_or(text);
        if text.contains("://") {
            return self.notice("Сетевые адреса будут на этапе 6 (sftp, smb через FUSE)", cx);
        }
        let path = match text.strip_prefix('~') {
            Some(rest) => places::home().join(rest.trim_start_matches('/')),
            None => match self.current_dir() {
                Some(cur) if !text.starts_with('/') => cur.join(text),
                _ => PathBuf::from(text),
            },
        };
        match std::fs::metadata(&path) {
            Ok(m) if m.is_dir() => self.navigate(Target::Dir(path), cx),
            Ok(_) => {
                let Some(parent) = path.parent().map(Path::to_path_buf) else { return };
                if Target::Dir(parent.clone()) != self.location {
                    self.back.push(self.location.clone());
                    self.forward.clear();
                }
                self.set_location(Target::Dir(parent), vec![path], cx);
            }
            Err(_) => self.notice(format!("Не найдено: {}", path.display()), cx),
        }
    }

    // ---------- прочие действия ----------

    fn escape(&mut self, _: &Escape, _: &mut Window, cx: &mut Context<Self>) {
        if self.menu.take().is_none() {
            if self.job.is_some() {
                self.cancel_job();
            } else {
                self.selected.clear();
            }
        }
        cx.notify();
    }

    pub(crate) fn select_all(&mut self, cx: &mut Context<Self>) {
        self.selected = self.visible.iter().map(|&i| self.rows[i].entry.path.clone()).collect();
        cx.notify();
    }

    pub(crate) fn toggle_hidden(&mut self, cx: &mut Context<Self>) {
        self.show_hidden = !self.show_hidden;
        self.rebuild_visible();
        let present: HashSet<PathBuf> = self.visible.iter().map(|&i| self.rows[i].entry.path.clone()).collect();
        self.selected.retain(|p| present.contains(p));
        self.scroll_to_cursor();
        self.config.show_hidden = self.show_hidden;
        self.save_settings(vec![("view.show-hidden", self.show_hidden.to_string())], cx);
        let state = if self.show_hidden { "показаны" } else { "скрыты" };
        self.notice(format!("Скрытые файлы {state} (Ctrl+H)"), cx);
    }

    pub(crate) fn location_title(&self) -> String {
        match &self.location {
            Target::Dir(p) if p.parent().is_none() => "Файловая система".into(),
            Target::Dir(p) => places::display_name(p),
            Target::Trash => "Корзина".into(),
        }
    }

    fn update_title(&mut self, window: &mut Window) {
        let title = format!("{} — conchy-files", self.location_title());
        if title != self.title {
            window.set_window_title(&title);
            self.title = title;
        }
    }
}

/// Читает папку или корзину и готовит строки (в фоне).
fn read_location(
    target: &Target,
    tops: &[PathBuf],
    sort: Sort,
    count_items: bool,
    special: &HashMap<PathBuf, String>,
) -> Result<Vec<Row>, String> {
    let entries = match target {
        Target::Dir(p) => dir::list(p, count_items).map_err(|e| match e.kind() {
            std::io::ErrorKind::PermissionDenied => "Нет доступа к этой папке".to_string(),
            std::io::ErrorKind::NotFound => "Папка не существует".to_string(),
            _ => e.to_string(),
        })?,
        Target::Trash => trash::list(tops)
            .into_iter()
            .filter_map(|l| {
                let mut e = Entry::from_path(&l.item.file).ok()?;
                e.name = places::display_name(&l.item.original);
                e.mime = mime::db().guess(&l.item.file, &e.name, e.is_dir);
                e.trash = Some(TrashInfo { original: l.item.original, deleted: l.deleted, info: l.item.info });
                Some(e)
            })
            .collect(),
    };
    let now = unsafe { libc::time(std::ptr::null_mut()) } as i64;
    let mut rows: Vec<Row> = entries
        .into_iter()
        .map(|e| {
            let (icon, kind) = icon_and_kind(&e, special);
            let size = match e.items {
                Some(n) if e.is_dir => items_label(n),
                _ if e.is_dir => "—".into(),
                _ => human_size(e.size),
            };
            let date = match &e.trash {
                Some(t) if t.deleted > 0 => dir::format_date(t.deleted, now),
                Some(_) => "—".into(),
                None => dir::format_date(e.mtime, now),
            };
            Row { entry: e, icon, kind, size, date }
        })
        .collect();
    dir::sort(&mut rows, sort);
    Ok(rows)
}

fn sort_from(c: &FilesConfig) -> Sort {
    let key = match c.sort_by {
        SortBy::Name => SortKey::Name,
        SortBy::Size => SortKey::Size,
        SortBy::Type => SortKey::Type,
        SortBy::Modified => SortKey::Modified,
    };
    Sort { key, ascending: c.sort_ascending, folders_first: c.folders_first }
}

/// Иконка и подпись колонки «Тип» (в корзине — исходная папка).
fn icon_and_kind(e: &Entry, special: &HashMap<PathBuf, String>) -> (Option<PathBuf>, String) {
    let db = mime::db();
    let names: Vec<String> = match special.get(&e.path) {
        Some(icon) if e.is_dir => vec![icon.clone(), "folder".into()],
        _ if e.is_dir => vec!["folder".into()],
        _ => db.icon_names(&e.mime),
    };
    let icon = icons::icons().lookup(&names, 16);
    let kind = match &e.trash {
        Some(t) => t.original.parent().map(|p| p.display().to_string()).unwrap_or_default(),
        None if e.broken => "Битая ссылка".into(),
        None if e.is_symlink => format!("Ссылка: {}", db.describe(&e.mime)),
        None => db.describe(&e.mime),
    };
    (icon, kind)
}

/// «1 элемент», «3 элемента», «7 элементов».
pub(crate) fn items_label(n: u64) -> String {
    let word = match (n % 10, n % 100) {
        (_, 11..=14) => "элементов",
        (1, _) => "элемент",
        (2..=4, _) => "элемента",
        _ => "элементов",
    };
    format!("{n} {word}")
}

/// Дополнение пути по Tab: общее начало подходящих имён в папке.
fn complete_path(text: &str) -> Option<String> {
    let expanded = match text.strip_prefix('~') {
        Some(rest) => format!("{}{}", places::home().display(), rest),
        None => text.to_string(),
    };
    let i = expanded.rfind('/')?;
    let (dir, prefix) = (&expanded[..=i], &expanded[i + 1..]);
    let mut names: Vec<(String, bool)> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            // Скрытые предлагаем, только если пользователь сам начал с точки.
            let ok = n.starts_with(prefix) && (prefix.starts_with('.') || !n.starts_with('.'));
            ok.then(|| (n, e.path().is_dir()))
        })
        .collect();
    names.sort();
    let first = names.first()?.0.clone();
    let common: String = names.iter().fold(first, |acc, (n, _)| {
        acc.chars().zip(n.chars()).take_while(|(a, b)| a == b).map(|(a, _)| a).collect()
    });
    let mut out = format!("{dir}{common}");
    if names.len() == 1 && names[0].1 {
        out.push('/');
    }
    (out != expanded).then_some(out)
}

impl Render for FilesWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.update_title(window);
        self.sync_panel(cx);
        let panel = self.render_panel(cx);
        let typing = self.dialog.is_some() || self.location_edit.is_some();
        let menu = self.render_menu(window, cx);
        let dialog = self.render_dialog(cx);
        div()
            .key_context(if typing { CTX_TEXT } else { CTX })
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .on_action(cx.listener(|this, _: &SelectNext, _, cx| this.move_cursor(1, false, cx)))
            .on_action(cx.listener(|this, _: &SelectPrev, _, cx| this.move_cursor(-1, false, cx)))
            .on_action(cx.listener(|this, _: &ExtendNext, _, cx| this.move_cursor(1, true, cx)))
            .on_action(cx.listener(|this, _: &ExtendPrev, _, cx| this.move_cursor(-1, true, cx)))
            .on_action(cx.listener(|this, _: &SelectFirst, _, cx| this.move_cursor(isize::MIN, false, cx)))
            .on_action(cx.listener(|this, _: &SelectLast, _, cx| this.move_cursor(isize::MAX, false, cx)))
            .on_action(cx.listener(|this, _: &PageDown, _, cx| this.move_cursor(15, false, cx)))
            .on_action(cx.listener(|this, _: &PageUp, _, cx| this.move_cursor(-15, false, cx)))
            .on_action(cx.listener(Self::open_selected))
            .on_action(cx.listener(Self::go_back))
            .on_action(cx.listener(Self::go_forward))
            .on_action(cx.listener(Self::go_up))
            .on_action(cx.listener(Self::go_home))
            .on_action(cx.listener(Self::edit_location))
            .on_action(cx.listener(|this, _: &ToggleHidden, _, cx| this.toggle_hidden(cx)))
            .on_action(cx.listener(|this, _: &Reload, _, cx| {
                this.places = places::places();
                this.mounts = places::mounts();
                this.reload(cx);
            }))
            .on_action(cx.listener(|_, _: &CloseWindow, window, _| window.remove_window()))
            .on_action(cx.listener(|this, _: &SelectAll, _, cx| this.select_all(cx)))
            .on_action(cx.listener(Self::escape))
            .on_action(cx.listener(|this, _: &CutFiles, _, cx| this.run_cmd(actions::Cmd::Cut, cx)))
            .on_action(cx.listener(|this, _: &CopyFiles, _, cx| this.run_cmd(actions::Cmd::Copy, cx)))
            .on_action(cx.listener(|this, _: &PasteFiles, _, cx| this.run_cmd(actions::Cmd::Paste, cx)))
            .on_action(cx.listener(|this, _: &RenameItem, _, cx| this.run_cmd(actions::Cmd::Rename, cx)))
            .on_action(cx.listener(|this, _: &NewFolder, _, cx| this.run_cmd(actions::Cmd::NewFolder, cx)))
            .on_action(cx.listener(|this, _: &TrashFiles, _, cx| this.run_cmd(actions::Cmd::Trash, cx)))
            .on_action(cx.listener(|this, _: &DeleteFiles, _, cx| this.run_cmd(actions::Cmd::Delete, cx)))
            .on_action(cx.listener(|this, _: &Undo, _, cx| this.run_cmd(actions::Cmd::Undo, cx)))
            .on_action(cx.listener(|this, _: &TogglePanel, _, cx| this.toggle_panel(cx)))
            .on_action(cx.listener(|this, _: &ShowPanel, _, cx| this.show_panel(cx)))
            // Перетаскивание края нижней панели.
            .on_mouse_move(cx.listener(|this, ev: &gpui::MouseMoveEvent, window, cx| {
                if let Some((y0, h0)) = this.panel.drag {
                    let max = (f32::from(window.viewport_size().height) - 180.0).max(140.0);
                    let h = (h0 + y0 - f32::from(ev.position.y)).clamp(120.0, max);
                    this.config.panel_height = h as u32;
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &gpui::MouseUpEvent, _, cx| {
                    if this.panel.drag.take().is_some() {
                        let h = this.config.panel_height.to_string();
                        this.save_settings(vec![("panel.height", h)], cx);
                    }
                }),
            )
            // Боковые кнопки мыши «назад» и «вперёд».
            .on_mouse_down(
                MouseButton::Navigate(NavigationDirection::Back),
                cx.listener(|this, _: &MouseDownEvent, w, cx| this.go_back(&GoBack, w, cx)),
            )
            .on_mouse_down(
                MouseButton::Navigate(NavigationDirection::Forward),
                cx.listener(|this, _: &MouseDownEvent, w, cx| this.go_forward(&GoForward, w, cx)),
            )
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .text_size(px(self.config.font_size))
            .child(self.render_toolbar(cx))
            .child(div().flex().flex_1().min_h_0().child(self.render_sidebar(cx)).child(self.render_list(cx)))
            .children(panel)
            .child(self.render_status(cx))
            .children(menu)
            .children(dialog)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn russian_plurals() {
        assert_eq!(items_label(1), "1 элемент");
        assert_eq!(items_label(3), "3 элемента");
        assert_eq!(items_label(11), "11 элементов");
        assert_eq!(items_label(12), "12 элементов");
        assert_eq!(items_label(22), "22 элемента");
        assert_eq!(items_label(21), "21 элемент");
        assert_eq!(items_label(0), "0 элементов");
    }

    #[test]
    fn completes_paths() {
        let d = std::env::temp_dir().join(format!("conchy-complete-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("Документы")).unwrap();
        std::fs::create_dir_all(d.join("Довольно")).unwrap();
        std::fs::write(d.join("Музыка.txt"), "").unwrap();
        let base = d.display().to_string();
        assert_eq!(complete_path(&format!("{base}/Док")), Some(format!("{base}/Документы/")));
        assert_eq!(complete_path(&format!("{base}/Д")), Some(format!("{base}/До")));
        assert_eq!(complete_path(&format!("{base}/Му")), Some(format!("{base}/Музыка.txt")));
        assert_eq!(complete_path(&format!("{base}/Я")), None);
        std::fs::remove_dir_all(&d).unwrap();
    }
}
