//! Логика пикера файлов для портала FileChooser (без GPUI): фильтры, стартовая папка,
//! запоминание последней папки приложения.
//!
//! Типы запросов и ответов — общий крейт `conchy-portal-proto`, чтобы демон портала и
//! файловый менеджер не расходились в формате.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

pub use conchy_portal_proto::files::*;
use kdl::{KdlDocument, KdlNode, KdlValue};

use crate::mime::{MimeDb, glob_match};

/// Сколько приложений помним в `files-picker.kdl`.
const MAX_REMEMBERED: usize = 64;

pub fn path_from_bytes(b: &[u8]) -> PathBuf {
    PathBuf::from(OsStr::from_bytes(b))
}

pub fn path_to_bytes(p: &Path) -> Vec<u8> {
    p.as_os_str().as_bytes().to_vec()
}

/// Подходит ли файл под правило фильтра. Маски сравниваются без учёта регистра
/// (`*.png` находит `PHOTO.PNG`), MIME — с учётом подклассов.
pub fn rule_matches(rule: &FilterRule, name: &str, mime: &str, db: &MimeDb) -> bool {
    match rule.kind {
        RULE_GLOB => glob_match(&rule.pattern, name) || glob_match(&rule.pattern.to_lowercase(), &name.to_lowercase()),
        RULE_MIME => db.is_a(mime, &rule.pattern),
        _ => false,
    }
}

/// Фильтр без правил пропускает всё; иначе достаточно одного совпавшего правила.
pub fn filter_matches(f: &Filter, name: &str, mime: &str, db: &MimeDb) -> bool {
    f.rules.is_empty() || f.rules.iter().any(|r| rule_matches(r, name, mime, db))
}

/// Подпись кнопки подтверждения. Портал присылает мнемонику GTK (`_Open`): одиночное `_`
/// убираем, `__` — это настоящее подчёркивание.
pub fn accept_label(req: &FilePickRequest) -> String {
    let given = strip_mnemonic(&req.accept_label);
    if !given.trim().is_empty() {
        return given;
    }
    match req.kind {
        KIND_SAVE => "Сохранить",
        KIND_SAVE_FILES => "Выбрать",
        _ if req.directory => "Выбрать",
        _ => "Открыть",
    }
    .to_string()
}

pub fn strip_mnemonic(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '_' {
            if it.peek() == Some(&'_') {
                out.push('_');
                it.next();
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Стартовая папка: `current_folder` → папка `current_file` → последняя папка приложения → дом.
pub fn start_folder(req: &FilePickRequest, last: Option<PathBuf>, home: &Path) -> PathBuf {
    let from_folder = (!req.current_folder.is_empty()).then(|| path_from_bytes(&req.current_folder));
    let from_file = (!req.current_file.is_empty())
        .then(|| path_from_bytes(&req.current_file))
        .and_then(|f| f.parent().map(Path::to_path_buf));
    [from_folder, from_file, last]
        .into_iter()
        .flatten()
        .find(|p| p.is_dir())
        .unwrap_or_else(|| home.to_path_buf())
}

/// Путь для сохранения: проверяет имя (без `/`, не `.`/`..`) и склеивает с папкой.
pub fn save_target(folder: &Path, name: &str) -> Result<PathBuf, String> {
    crate::ops::validate_name(name)?;
    Ok(folder.join(name))
}

/// Свободное имя в папке для SaveFiles: `a.txt` занято → `a (1).txt`, `a (2).txt`…
pub fn unique_name(folder: &Path, name: &str) -> String {
    let taken = |n: &str| std::fs::symlink_metadata(folder.join(n)).is_ok();
    if !taken(name) {
        return name.to_string();
    }
    // Расширение — после последней точки, но не у скрытого `.bashrc`.
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => name.split_at(i),
        _ => (name, ""),
    };
    (1..)
        .map(|n| format!("{stem} ({n}){ext}"))
        .find(|n| !taken(n))
        .unwrap_or_else(|| name.to_string())
}

/// Начальное значение выбора: заданное порталом, иначе первый вариант / «false» для чекбокса.
pub fn choice_initial(c: &Choice) -> String {
    if !c.initial.is_empty() {
        return c.initial.clone();
    }
    c.options.first().map_or_else(|| "false".to_string(), |o| o.id.clone())
}

/// `~/.local/state/conchy/files-picker.kdl` (или `$XDG_STATE_HOME/conchy/…`).
pub fn state_file() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| crate::places::home().join(".local/state"));
    base.join("conchy/files-picker.kdl")
}

/// Последняя папка приложения: узлы вида `folder "app.id" "/путь"`.
pub fn last_folder(file: &Path, app_id: &str) -> Option<PathBuf> {
    let doc: KdlDocument = std::fs::read_to_string(file).ok()?.parse().ok()?;
    doc.nodes().iter().rev().find_map(|n| {
        let mut args = n.entries().iter().filter(|e| e.name().is_none()).map(|e| e.value());
        match (n.name().value(), args.next(), args.next()) {
            ("folder", Some(KdlValue::String(a)), Some(KdlValue::String(p))) if a == app_id => Some(PathBuf::from(p)),
            _ => None,
        }
    })
}

/// Запоминает папку приложения. Пути не из UTF-8 пропускаются (KDL хранит только текст).
pub fn remember_folder(file: &Path, app_id: &str, folder: &Path) -> std::io::Result<()> {
    let Some(folder) = folder.to_str() else { return Ok(()) };
    let mut doc: KdlDocument = std::fs::read_to_string(file).ok().and_then(|t| t.parse().ok()).unwrap_or_default();
    let nodes = doc.nodes_mut();
    nodes.retain(|n| {
        let first = n.entries().iter().find(|e| e.name().is_none()).map(|e| e.value());
        !(n.name().value() == "folder" && matches!(first, Some(KdlValue::String(a)) if a == app_id))
    });
    let mut node = KdlNode::new("folder");
    node.push(KdlValue::String(app_id.to_string()));
    node.push(KdlValue::String(folder.to_string()));
    nodes.push(node);
    if nodes.len() > MAX_REMEMBERED {
        let drop = nodes.len() - MAX_REMEMBERED;
        nodes.drain(..drop);
    }
    doc.fmt();
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = file.with_extension("kdl.tmp");
    std::fs::write(&tmp, doc.to_string())?;
    std::fs::rename(tmp, file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> MimeDb {
        MimeDb::load(&[PathBuf::from("/usr/share")])
    }

    fn rule(kind: u32, p: &str) -> FilterRule {
        FilterRule { kind, pattern: p.into() }
    }

    #[test]
    fn filters_by_glob_and_mime() {
        let db = db();
        let f = Filter { name: "Images".into(), rules: vec![rule(RULE_GLOB, "*.png"), rule(RULE_MIME, "image/jpeg")] };
        assert!(filter_matches(&f, "a.png", "image/png", &db));
        assert!(filter_matches(&f, "PHOTO.PNG", "image/png", &db), "регистр не важен для масок");
        assert!(filter_matches(&f, "x.jpg", "image/jpeg", &db));
        assert!(!filter_matches(&f, "x.txt", "text/plain", &db));
        assert!(filter_matches(&Filter::default(), "anything", "x/y", &db), "пустой фильтр = всё");
    }

    #[test]
    fn mime_filter_respects_subclasses() {
        if !Path::new("/usr/share/mime/subclasses").exists() {
            return;
        }
        let db = db();
        let text = Filter { name: "Text".into(), rules: vec![rule(RULE_MIME, "text/plain")] };
        // Исходники C — подкласс text/plain (и через subclasses, и по правилу text/*).
        assert!(filter_matches(&text, "main.c", "text/x-csrc", &db));
        assert!(filter_matches(&text, "data.json", "application/json", &db), "json → text/plain по subclasses");
        assert!(!filter_matches(&text, "a.png", "image/png", &db));
    }

    #[test]
    fn labels() {
        let mut r = FilePickRequest { accept_label: "_Open".into(), ..Default::default() };
        assert_eq!(accept_label(&r), "Open");
        r.accept_label = "Save __as".into();
        assert_eq!(accept_label(&r), "Save _as");
        r.accept_label.clear();
        assert_eq!(accept_label(&r), "Открыть");
        r.directory = true;
        assert_eq!(accept_label(&r), "Выбрать");
        r.kind = KIND_SAVE;
        assert_eq!(accept_label(&r), "Сохранить");
    }

    #[test]
    fn start_folder_priority() {
        let d = std::env::temp_dir().join(format!("conchy-pick-{}", std::process::id()));
        let (a, b, last) = (d.join("a"), d.join("b"), d.join("last"));
        for p in [&a, &b, &last] {
            std::fs::create_dir_all(p).unwrap();
        }
        let home = d.join("home");
        let mut r = FilePickRequest { current_folder: path_to_bytes(&a), ..Default::default() };
        assert_eq!(start_folder(&r, Some(last.clone()), &home), a);
        r.current_folder = path_to_bytes(&d.join("missing"));
        r.current_file = path_to_bytes(&b.join("f.txt"));
        assert_eq!(start_folder(&r, Some(last.clone()), &home), b, "папка current_file");
        r.current_file.clear();
        assert_eq!(start_folder(&r, Some(last.clone()), &home), last);
        assert_eq!(start_folder(&r, None, &home), home);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn remembers_last_folder_per_app() {
        let d = std::env::temp_dir().join(format!("conchy-pick-state-{}", std::process::id()));
        let f = d.join("sub/files-picker.kdl");
        assert_eq!(last_folder(&f, "org.app"), None);
        remember_folder(&f, "org.app", Path::new("/home/u/Загрузки")).unwrap();
        remember_folder(&f, "other", Path::new("/tmp")).unwrap();
        remember_folder(&f, "org.app", Path::new("/home/u/Документы")).unwrap();
        assert_eq!(last_folder(&f, "org.app"), Some(PathBuf::from("/home/u/Документы")));
        assert_eq!(last_folder(&f, "other"), Some(PathBuf::from("/tmp")));
        let text = std::fs::read_to_string(&f).unwrap();
        assert_eq!(text.matches("org.app").count(), 1, "запись приложения одна: {text}");
        for i in 0..80 {
            remember_folder(&f, &format!("app{i}"), Path::new("/x")).unwrap();
        }
        assert!(std::fs::read_to_string(&f).unwrap().lines().count() <= MAX_REMEMBERED);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn save_target_validates() {
        assert_eq!(save_target(Path::new("/tmp"), "a.txt").unwrap(), PathBuf::from("/tmp/a.txt"));
        assert!(save_target(Path::new("/tmp"), "a/b").is_err());
        assert!(save_target(Path::new("/tmp"), "").is_err());
        assert!(save_target(Path::new("/tmp"), "..").is_err());
    }

    #[test]
    fn unique_names_for_save_files() {
        let d = std::env::temp_dir().join(format!("conchy-pick-uniq-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        assert_eq!(unique_name(&d, "a.txt"), "a.txt");
        std::fs::write(d.join("a.txt"), "").unwrap();
        assert_eq!(unique_name(&d, "a.txt"), "a (1).txt");
        std::fs::write(d.join("a (1).txt"), "").unwrap();
        assert_eq!(unique_name(&d, "a.txt"), "a (2).txt");
        std::fs::write(d.join(".rc"), "").unwrap();
        assert_eq!(unique_name(&d, ".rc"), ".rc (1)");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn choice_defaults() {
        let check = Choice { id: "c".into(), ..Default::default() };
        assert_eq!(choice_initial(&check), "false");
        let combo = Choice {
            id: "e".into(),
            options: vec![ChoiceOption { id: "u".into(), label: "UTF-8".into() }],
            ..Default::default()
        };
        assert_eq!(choice_initial(&combo), "u");
        let preset = Choice { initial: "x".into(), ..combo };
        assert_eq!(choice_initial(&preset), "x");
    }
}
