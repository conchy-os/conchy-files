//! Перетаскивание файлов мышью, как в Thunar: на папку, на пустое место,
//! на «Места», устройства и корзину. Принимаются и файлы из других программ.
//!
//! Внутри одного раздела файлы переносятся, между разделами — копируются.
//! `Ctrl` — всегда копировать, `Shift` — всегда переносить.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use conchy_files::ops::{self, Mode};
use conchy_files::places::Target;
use gpui::{Context, ExternalPaths, Modifiers, Window, div, prelude::*, px, rgb};

use super::actions::JobKind;
use super::{ACCENT, BG, FilesWindow};

/// Что тащат внутри окна.
#[derive(Clone)]
pub(super) struct DraggedFiles {
    pub paths: Vec<PathBuf>,
}

/// Картинка под курсором во время перетаскивания.
pub(super) struct DragPreview {
    label: String,
}

impl DragPreview {
    pub fn new(d: &DraggedFiles) -> Self {
        let label = match &d.paths[..] {
            [one] => ops::display_name(one),
            many => super::items_label(many.len() as u64),
        };
        DragPreview { label }
    }
}

impl Render for DragPreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .rounded_md()
            .bg(rgb(ACCENT))
            .text_color(rgb(BG))
            .text_size(px(12.))
            .child(self.label.clone())
    }
}

/// Один раздел (файловая система) у двух путей — тогда перенос мгновенный.
fn same_device(a: &Path, b: &Path) -> bool {
    match (std::fs::symlink_metadata(a), std::fs::metadata(b)) {
        (Ok(x), Ok(y)) => x.dev() == y.dev(),
        _ => false,
    }
}

/// Что делать с брошенными файлами: какие брать и переносить или копировать.
/// `None` — ничего не делать (бросили туда, где лежат, или папку в саму себя).
fn decide(paths: Vec<PathBuf>, dest: &Path, m: Modifiers, external: bool) -> Option<(Vec<PathBuf>, Mode)> {
    let paths: Vec<PathBuf> = paths.into_iter().filter(|p| !dest.starts_with(p)).collect();
    if paths.is_empty() || (paths.iter().all(|p| p.parent() == Some(dest)) && !m.control) {
        return None;
    }
    let mode = if m.control {
        Mode::Copy
    } else if m.shift {
        Mode::Move
    } else if external {
        Mode::Copy
    } else if paths.iter().all(|p| same_device(p, dest)) {
        Mode::Move
    } else {
        Mode::Copy
    };
    Some((paths, mode))
}

impl FilesWindow {
    /// Бросили файлы на `target`. `external` — пришли из другой программы.
    pub(super) fn drop_files(&mut self, paths: Vec<PathBuf>, target: Target, m: Modifiers, external: bool, cx: &mut Context<Self>) {
        let dest = match target {
            Target::Trash => {
                if external {
                    return self.notice("Файлы из других программ в корзину не перетаскиваются", cx);
                }
                return self.run_job("Перемещение в корзину", JobKind::Normal, cx, move |p, _| ops::trash_all(&paths, p));
            }
            Target::Dir(d) => d,
        };
        let Some((paths, mode)) = decide(paths, &dest, m, external) else { return };
        let title = if mode == Mode::Move { "Перемещение" } else { "Копирование" };
        self.run_job(title, JobKind::Normal, cx, move |p, a| ops::transfer(mode, &paths, &dest, p, a));
    }
}

/// Делает элемент целью для брошенных файлов (своих и из других программ).
pub(super) fn drop_target<E: InteractiveElement + Styled>(el: E, target: Target, cx: &mut Context<FilesWindow>) -> E {
    let t1 = target.clone();
    let t2 = target;
    el.drag_over::<DraggedFiles>(|s, _, _, _| s.bg(gpui::hsla(0.61, 0.85, 0.72, 0.22)).border_color(rgb(ACCENT)))
        .drag_over::<ExternalPaths>(|s, _, _, _| s.bg(gpui::hsla(0.61, 0.85, 0.72, 0.22)).border_color(rgb(ACCENT)))
        .on_drop(cx.listener(move |this, d: &DraggedFiles, window, cx| {
            cx.stop_propagation();
            this.drop_files(d.paths.clone(), t1.clone(), window.modifiers(), false, cx);
        }))
        .on_drop(cx.listener(move |this, d: &ExternalPaths, window, cx| {
            cx.stop_propagation();
            this.drop_files(d.paths().to_vec(), t2.clone(), window.modifiers(), true, cx);
        }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn move_or_copy_like_thunar() {
        let d = std::env::temp_dir().join(format!("conchy-dnd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("a/sub")).unwrap();
        std::fs::create_dir_all(d.join("b")).unwrap();
        std::fs::write(d.join("a/f"), "").unwrap();
        let none = Modifiers::default();
        let ctrl = Modifiers { control: true, ..Default::default() };
        let f = vec![d.join("a/f")];
        assert_eq!(decide(f.clone(), &d.join("b"), none, false).map(|x| x.1), Some(Mode::Move), "тот же раздел — перенос");
        assert_eq!(decide(f.clone(), &d.join("b"), ctrl, false).map(|x| x.1), Some(Mode::Copy), "Ctrl — копия");
        assert_eq!(decide(f.clone(), &d.join("b"), none, true).map(|x| x.1), Some(Mode::Copy), "из другой программы — копия");
        assert!(decide(f.clone(), &d.join("a"), none, false).is_none(), "туда, где лежит, — ничего");
        assert_eq!(decide(f, &d.join("a"), ctrl, false).map(|x| x.1), Some(Mode::Copy), "Ctrl на ту же папку — дубликат");
        assert!(decide(vec![d.join("a")], &d.join("a/sub"), none, false).is_none(), "папку в саму себя нельзя");
        std::fs::remove_dir_all(&d).unwrap();
    }
}
