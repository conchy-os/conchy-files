//! Однострочное поле ввода для диалогов (имя файла, новая папка).
//!
//! В GPUI нет готового текстового поля, поэтому здесь минимальное: ввод символов,
//! выделение, стрелки, Home/End, Backspace/Delete, Ctrl+A, вставка из буфера.

use std::ops::Range;

use gpui::{Keystroke, div, prelude::*, px, rgb};

use super::{ACCENT, BG, BORDER, SEL_BG, TEXT};

pub struct TextField {
    pub text: String,
    /// Выделение в байтах; пустое — это просто курсор в `sel.end`.
    sel: Range<usize>,
}

impl TextField {
    /// Поле с текстом и выделенным диапазоном (например, имя без расширения, как в Thunar).
    pub fn new(text: impl Into<String>, select: Range<usize>) -> Self {
        let text = text.into();
        let end = select.end.min(text.len());
        let start = select.start.min(end);
        Self { text, sel: start..end }
    }

    pub fn insert(&mut self, s: &str) {
        let s: String = s.chars().filter(|c| !c.is_control()).collect();
        self.text.replace_range(self.sel.clone(), &s);
        let at = self.sel.start + s.len();
        self.sel = at..at;
    }

    /// Обрабатывает клавишу. Возвращает `true`, если клавиша относилась к полю.
    pub fn key(&mut self, ks: &Keystroke) -> bool {
        let m = &ks.modifiers;
        match ks.key.as_str() {
            "a" if m.control => self.sel = 0..self.text.len(),
            "left" => {
                let at = if self.sel.is_empty() { self.prev(self.sel.start) } else { self.sel.start };
                self.sel = at..at;
            }
            "right" => {
                let at = if self.sel.is_empty() { self.next(self.sel.end) } else { self.sel.end };
                self.sel = at..at;
            }
            "home" => self.sel = 0..0,
            "end" => self.sel = self.text.len()..self.text.len(),
            "backspace" => {
                if self.sel.is_empty() {
                    self.sel.start = self.prev(self.sel.start);
                }
                self.insert("");
            }
            "delete" => {
                if self.sel.is_empty() {
                    self.sel.end = self.next(self.sel.end);
                }
                self.insert("");
            }
            _ => match &ks.key_char {
                Some(ch) if !m.control && !m.alt && !m.platform => self.insert(ch),
                _ => return false,
            },
        }
        true
    }

    fn prev(&self, i: usize) -> usize {
        self.text[..i].char_indices().next_back().map_or(0, |(j, _)| j)
    }

    fn next(&self, i: usize) -> usize {
        self.text[i..].chars().next().map_or(i, |c| i + c.len_utf8())
    }

    pub fn render(&self) -> impl IntoElement {
        let before = self.text[..self.sel.start].to_string();
        let selected = self.text[self.sel.clone()].to_string();
        let after = self.text[self.sel.end..].to_string();
        let caret = div().w(px(1.)).h(px(15.)).flex_shrink_0().bg(rgb(TEXT));
        div()
            .flex()
            .items_center()
            .w_full()
            .h(px(28.))
            .px_2()
            .overflow_hidden()
            .whitespace_nowrap()
            .bg(rgb(BG))
            .border_1()
            .border_color(rgb(ACCENT))
            .rounded_md()
            .text_color(rgb(TEXT))
            .child(div().flex_shrink_0().child(before))
            .when(selected.is_empty(), |d| d.child(caret))
            .when(!selected.is_empty(), |d| d.child(div().flex_shrink_0().bg(rgb(SEL_BG)).child(selected)))
            .child(div().flex_shrink_0().child(after))
            .when(self.text.is_empty(), |d| d.border_color(rgb(BORDER)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ks(key: &str, ch: Option<&str>) -> Keystroke {
        Keystroke { key: key.into(), key_char: ch.map(Into::into), ..Default::default() }
    }

    #[test]
    fn edits_utf8_text() {
        // Как в Thunar: выделено имя без расширения, ввод заменяет его.
        let mut f = TextField::new("отчёт.txt", 0.."отчёт".len());
        f.key(&ks("и", Some("и")));
        assert_eq!(f.text, "и.txt");
        f.key(&ks("end", None));
        f.key(&ks("backspace", None));
        assert_eq!(f.text, "и.tx");
        f.key(&ks("home", None));
        f.key(&ks("delete", None));
        assert_eq!(f.text, ".tx");
        f.key(&ks("right", None));
        f.key(&ks("ж", Some("ж")));
        assert_eq!(f.text, ".жtx");
        assert!(!f.key(&ks("escape", None)));
    }
}
