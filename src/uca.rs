//! Пользовательские действия в контекстном меню (аналог Custom Actions из Thunar),
//! описываются в `files.kdl`:
//!
//! ```kdl
//! actions {
//!     action "Открыть в VS Code" {
//!         command "code %F"
//!         directories true
//!         background true      // и в меню пустого места — для текущей папки
//!     }
//!     action "Уменьшить картинку" {
//!         command "magick %f -resize 50% %d/small-%n"
//!         types "image/*"
//!     }
//! }
//! ```
//!
//! Подстановки, как в Thunar: `%f` — первый путь, `%F` — все пути, `%d` — папка
//! первого, `%D` — папки всех, `%n` — имя первого, `%N` — все имена, `%%` — знак `%`.
//! Всё подставляется в кавычках оболочки, поэтому пробелы и кавычки в именах безопасны.

use std::path::{Path, PathBuf};

use kdl::{KdlDocument, KdlNode, KdlValue};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomAction {
    pub name: String,
    pub command: String,
    /// MIME-шаблоны (`image/*`, `text/plain`); `*` — любые файлы.
    pub types: Vec<String>,
    pub files: bool,
    pub directories: bool,
    /// Показывать и в меню пустого места (тогда `%f` — текущая папка).
    pub background: bool,
}

/// Действия из блока `actions` документа `files.kdl`. Неполные записи пропускаются.
pub fn parse(doc: &KdlDocument) -> Vec<CustomAction> {
    let Some(block) = doc.get("actions").and_then(KdlNode::children) else { return Vec::new() };
    block
        .nodes()
        .iter()
        .filter(|n| n.name().value() == "action")
        .filter_map(|n| {
            let name = n.entries().first().and_then(|e| str_of(e.value()))?;
            let c = n.children()?;
            let command = c.get_arg("command").and_then(str_of).filter(|s| !s.trim().is_empty())?;
            let types: Vec<String> = c
                .get("types")
                .map(|t| t.entries().iter().filter_map(|e| str_of(e.value())).collect())
                .filter(|v: &Vec<String>| !v.is_empty())
                .unwrap_or_else(|| vec!["*".into()]);
            let flag = |k: &str, default: bool| match c.get_arg(k) {
                Some(KdlValue::Bool(b)) => *b,
                _ => default,
            };
            let directories = flag("directories", false);
            Some(CustomAction {
                name,
                command,
                types,
                // По умолчанию — для файлов; если указаны только папки, то файлы не нужны.
                files: flag("files", !directories || c.get("types").is_some()),
                directories,
                background: flag("background", false),
            })
        })
        .collect()
}

fn str_of(v: &KdlValue) -> Option<String> {
    match v {
        KdlValue::String(s) | KdlValue::RawString(s) => Some(s.clone()),
        _ => None,
    }
}

impl CustomAction {
    /// Подходит ли к выделенному: `(путь, это папка, MIME)` для каждого элемента.
    pub fn applies(&self, items: &[(PathBuf, bool, String)]) -> bool {
        !items.is_empty()
            && items.iter().all(|(_, is_dir, mime)| {
                if *is_dir {
                    self.directories
                } else {
                    self.files && self.types.iter().any(|t| t == "*" || crate::mime::glob_match(t, mime))
                }
            })
    }

    /// Команда для `sh -c` с подставленными путями.
    pub fn expand(&self, paths: &[PathBuf]) -> String {
        let q = |s: &str| format!("'{}'", s.replace('\'', r"'\''"));
        let name = |p: &Path| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let dir = |p: &Path| p.parent().map(|d| d.display().to_string()).unwrap_or_default();
        let all = |f: &dyn Fn(&Path) -> String| paths.iter().map(|p| q(&f(p))).collect::<Vec<_>>().join(" ");
        let first = paths.first().map(PathBuf::as_path);
        let mut out = String::new();
        let mut chars = self.command.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '%' {
                out.push(c);
                continue;
            }
            match chars.next() {
                Some('f') => out.push_str(&first.map(|p| q(&p.display().to_string())).unwrap_or_default()),
                Some('F') => out.push_str(&all(&|p| p.display().to_string())),
                Some('d') => out.push_str(&first.map(|p| q(&dir(p))).unwrap_or_default()),
                Some('D') => out.push_str(&all(&dir)),
                Some('n') => out.push_str(&first.map(|p| q(&name(p))).unwrap_or_default()),
                Some('N') => out.push_str(&all(&name)),
                Some('%') => out.push('%'),
                Some(other) => {
                    out.push('%');
                    out.push(other);
                }
                None => out.push('%'),
            }
        }
        out
    }

    /// Запускает в фоне через `sh -c` в папке `cwd`.
    pub fn run(&self, paths: &[PathBuf], cwd: &Path) -> std::io::Result<()> {
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg(self.expand(paths))
            .current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .spawn()?;
        std::thread::spawn(move || child.wait());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = r#"
        view {
            show-hidden false
        }
        actions {
            action "VS Code" {
                command "code %F"
                directories true
                background true
            }
            action "Уменьшить" {
                command "magick %f -resize 50%% %d/small-%n"
                types "image/*" "application/pdf"
            }
            action "Без команды" {
                types "*"
            }
        }
    "#;

    #[test]
    fn parses_and_matches() {
        let acts = parse(&DOC.parse().unwrap());
        assert_eq!(acts.len(), 2, "запись без команды пропущена");
        let (code, shrink) = (&acts[0], &acts[1]);
        assert!(code.directories && code.background && !code.files, "только папки");
        assert!(shrink.files && !shrink.directories);

        let img = (PathBuf::from("/p/a.png"), false, "image/png".to_string());
        let txt = (PathBuf::from("/p/b.txt"), false, "text/plain".to_string());
        let dir = (PathBuf::from("/p/d"), true, "inode/directory".to_string());
        assert!(shrink.applies(&[img.clone()]));
        assert!(!shrink.applies(&[img.clone(), txt]), "всё выделенное должно подходить");
        assert!(code.applies(&[dir.clone()]) && !code.applies(&[img]));
        assert!(!code.applies(&[]));
    }

    #[test]
    fn expands_safely() {
        let a = CustomAction {
            name: "x".into(),
            command: "magick %f -resize 50%% %d/small-%n && echo %N %q".into(),
            types: vec!["*".into()],
            files: true,
            directories: false,
            background: false,
        };
        let paths = [PathBuf::from("/home/u/My Pics/it's.png"), PathBuf::from("/tmp/b.png")];
        assert_eq!(
            a.expand(&paths),
            r"magick '/home/u/My Pics/it'\''s.png' -resize 50% '/home/u/My Pics'/small-'it'\''s.png' && echo 'it'\''s.png' 'b.png' %q"
        );
        // Проверяем, что оболочка понимает это как задумано.
        let out = std::process::Command::new("sh").arg("-c").arg(CustomAction { command: "printf '%s|' %F".into(), ..a.clone() }.expand(&paths)).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "/home/u/My Pics/it's.png|/tmp/b.png|");
    }
}
