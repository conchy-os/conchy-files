//! Типы файлов по базе shared-mime-info (`/usr/share/mime`), как у GIO.
//!
//! Тип определяется по имени (`globs2`). Если имя ничего не говорит, смотрим
//! начало файла: текст без нулевых байтов — `text/plain`. Описание типа
//! («Изображение PNG») берётся из `<тип>.xml` на языке из `LANG`.

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

pub const DIRECTORY: &str = "inode/directory";
pub const SYMLINK: &str = "inode/symlink";
pub const UNKNOWN: &str = "application/octet-stream";
pub const TEXT: &str = "text/plain";

#[derive(Debug, Default)]
pub struct MimeDb {
    /// Точные имена: `Makefile` → `text/x-makefile`.
    literal: HashMap<String, (u32, String)>,
    /// Простые расширения `*.ext` (в нижнем регистре, без точки).
    ext: HashMap<String, (u32, String)>,
    /// Расширения, где важен регистр (`*.C`).
    ext_cs: HashMap<String, (u32, String)>,
    /// Остальные шаблоны: `README*`, `*.[ch]`.
    patterns: Vec<(u32, String, String)>,
    aliases: HashMap<String, String>,
    generic_icons: HashMap<String, String>,
    dirs: Vec<PathBuf>,
    comments: Mutex<HashMap<String, String>>,
    lang: String,
}

/// База из системных каталогов (`$XDG_DATA_HOME`, `$XDG_DATA_DIRS`). Загружается один раз.
pub fn db() -> &'static MimeDb {
    static DB: OnceLock<MimeDb> = OnceLock::new();
    DB.get_or_init(|| MimeDb::load(&data_dirs()))
}

fn data_dirs() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(h) = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).filter(|p| p.is_absolute()) {
        v.push(h);
    } else if let Some(home) = std::env::var_os("HOME") {
        v.push(PathBuf::from(home).join(".local/share"));
    }
    let sys = std::env::var("XDG_DATA_DIRS").unwrap_or_default();
    let sys = if sys.is_empty() { "/usr/local/share:/usr/share".to_string() } else { sys };
    v.extend(sys.split(':').filter(|s| !s.is_empty()).map(PathBuf::from));
    v
}

impl MimeDb {
    /// Читает базы из `<dir>/mime/`. Каталоги в начале списка важнее.
    pub fn load(data_dirs: &[PathBuf]) -> Self {
        let mut db = MimeDb { lang: lang_from_env(), ..Default::default() };
        // Идём с конца, чтобы пользовательские записи перекрывали системные.
        for d in data_dirs.iter().rev() {
            let m = d.join("mime");
            if !m.is_dir() {
                continue;
            }
            if let Ok(s) = fs::read_to_string(m.join("globs2")) {
                db.add_globs2(&s);
            }
            if let Ok(s) = fs::read_to_string(m.join("aliases")) {
                for (a, b) in s.lines().filter_map(|l| l.split_once(' ')) {
                    db.aliases.insert(a.to_string(), b.to_string());
                }
            }
            if let Ok(s) = fs::read_to_string(m.join("generic-icons")) {
                for (a, b) in s.lines().filter_map(|l| l.split_once(':')) {
                    db.generic_icons.insert(a.to_string(), b.to_string());
                }
            }
            db.dirs.insert(0, m);
        }
        db
    }

    fn add_globs2(&mut self, text: &str) {
        for line in text.lines().filter(|l| !l.starts_with('#')) {
            let mut it = line.splitn(4, ':');
            let (Some(w), Some(mime), Some(glob)) = (it.next(), it.next(), it.next()) else { continue };
            let Ok(weight) = w.parse::<u32>() else { continue };
            if glob == "__NOGLOBS__" {
                continue;
            }
            let cs = it.next().is_some_and(|f| f.split(',').any(|f| f == "cs"));
            let entry = (weight, mime.to_string());
            let keep = |map: &mut HashMap<String, (u32, String)>, key: String| {
                let e = map.entry(key).or_insert_with(|| entry.clone());
                if e.0 < entry.0 {
                    *e = entry.clone();
                }
            };
            match glob.strip_prefix("*.") {
                Some(ext) if !ext.contains(['*', '?', '[']) => {
                    if cs {
                        keep(&mut self.ext_cs, ext.to_string());
                    } else {
                        keep(&mut self.ext, ext.to_lowercase());
                    }
                }
                _ if !glob.contains(['*', '?', '[']) => keep(&mut self.literal, glob.to_string()),
                _ => self.patterns.push((weight, glob.to_string(), mime.to_string())),
            }
        }
        self.patterns.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.len().cmp(&a.1.len())));
    }

    /// Тип по имени файла, без чтения содержимого.
    pub fn by_name(&self, name: &str) -> Option<&str> {
        if let Some((_, m)) = self.literal.get(name) {
            return Some(m);
        }
        // Самое длинное расширение побеждает: `a.tar.gz` → сначала `tar.gz`, потом `gz`.
        for (i, _) in name.match_indices('.') {
            let tail = &name[i + 1..];
            if let Some((_, m)) = self.ext_cs.get(tail).or_else(|| self.ext.get(&tail.to_lowercase())) {
                return Some(m);
            }
        }
        let lower = name.to_lowercase();
        self.patterns.iter().find(|(_, g, _)| glob_match(g, name) || glob_match(g, &lower)).map(|(_, _, m)| m.as_str())
    }

    /// Тип файла: по имени, а если не вышло — по первым байтам.
    pub fn guess(&self, path: &Path, name: &str, is_dir: bool) -> String {
        if is_dir {
            return DIRECTORY.into();
        }
        if let Some(m) = self.by_name(name) {
            return self.unalias(m).to_string();
        }
        sniff(path).to_string()
    }

    pub fn unalias<'a>(&'a self, mime: &'a str) -> &'a str {
        self.aliases.get(mime).map_or(mime, String::as_str)
    }

    /// Имена иконок от точной к общей: `image-png`, `image-x-generic`.
    pub fn icon_names(&self, mime: &str) -> Vec<String> {
        let mut v = vec![mime.replace('/', "-")];
        if let Some(g) = self.generic_icons.get(mime) {
            v.push(g.clone());
        }
        let media = mime.split('/').next().unwrap_or("");
        let generic = match media {
            "inode" if mime == DIRECTORY => "folder".to_string(),
            "inode" => "inode-x-generic".to_string(),
            "application" => "application-x-generic".to_string(),
            m => format!("{m}-x-generic"),
        };
        if !v.contains(&generic) {
            v.push(generic);
        }
        if mime != UNKNOWN && mime != DIRECTORY {
            v.push("text-x-generic".into());
        }
        v
    }

    /// Описание типа на языке пользователя: «Изображение PNG».
    pub fn describe(&self, mime: &str) -> String {
        if let Some(c) = self.comments.lock().unwrap().get(mime) {
            return c.clone();
        }
        let c = self.read_comment(mime).unwrap_or_else(|| mime.to_string());
        self.comments.lock().unwrap().insert(mime.to_string(), c.clone());
        c
    }

    fn read_comment(&self, mime: &str) -> Option<String> {
        let xml = self.dirs.iter().find_map(|d| fs::read_to_string(d.join(format!("{mime}.xml"))).ok())?;
        let lang_tag = format!("<comment xml:lang=\"{}\">", self.lang);
        let pick = |tag: &str| {
            let start = xml.find(tag)? + tag.len();
            let end = xml[start..].find("</comment>")? + start;
            Some(unescape(&xml[start..end]))
        };
        (!self.lang.is_empty()).then(|| pick(&lang_tag)).flatten().or_else(|| pick("<comment>"))
    }
}

/// Язык из `LC_ALL`/`LC_MESSAGES`/`LANG`: `ru_RU.UTF-8` → `ru`.
fn lang_from_env() -> String {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .filter_map(|k| std::env::var(k).ok())
        .find(|v| !v.is_empty())
        .map(|v| v.split(['_', '.', '@']).next().unwrap_or("").to_string())
        .filter(|l| l != "C" && l != "POSIX")
        .unwrap_or_default()
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

/// Текст или двоичные данные — по первым 512 байтам, как делает GIO для неизвестных имён.
fn sniff(path: &Path) -> &'static str {
    let mut buf = [0u8; 512];
    let n = match fs::File::open(path).and_then(|mut f| f.read(&mut buf)) {
        Ok(n) => n,
        Err(_) => return UNKNOWN,
    };
    let head = &buf[..n];
    if n == 0 {
        return TEXT;
    }
    if head.contains(&0) {
        return UNKNOWN;
    }
    // Последний символ мог обрезаться посередине.
    match std::str::from_utf8(head) {
        Ok(_) => TEXT,
        Err(e) if e.valid_up_to() + 4 > n && e.error_len().is_none() => TEXT,
        Err(_) => UNKNOWN,
    }
}

/// Шаблон оболочки: `*`, `?`, `[abc]`.
pub fn glob_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let s: Vec<char> = name.chars().collect();
    let (mut pi, mut si) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while si < s.len() {
        if pi < p.len() {
            match p[pi] {
                '*' => {
                    star = Some(pi);
                    mark = si;
                    pi += 1;
                    continue;
                }
                '?' => {
                    pi += 1;
                    si += 1;
                    continue;
                }
                '[' => {
                    if let Some(end) = p[pi + 1..].iter().position(|&c| c == ']').map(|e| pi + 1 + e) {
                        if p[pi + 1..end].contains(&s[si]) {
                            pi = end + 1;
                            si += 1;
                            continue;
                        }
                    }
                }
                c if c == s[si] => {
                    pi += 1;
                    si += 1;
                    continue;
                }
                _ => {}
            }
        }
        match star {
            Some(st) => {
                pi = st + 1;
                mark += 1;
                si = mark;
            }
            None => return false,
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> MimeDb {
        let mut db = MimeDb::default();
        db.add_globs2(
            "# comment\n50:image/png:*.png\n50:application/gzip:*.gz\n55:application/x-compressed-tar:*.tar.gz\n\
             50:text/x-makefile:Makefile\n10:text/x-readme:README*\n50:text/x-c++src:*.C:cs\n50:text/x-csrc:*.c\n",
        );
        db.generic_icons.insert("application/gzip".into(), "package-x-generic".into());
        db
    }

    #[test]
    fn matches_like_gio() {
        let db = test_db();
        assert_eq!(db.by_name("photo.PNG"), Some("image/png"));
        assert_eq!(db.by_name("a.tar.gz"), Some("application/x-compressed-tar"), "длинное расширение первым");
        assert_eq!(db.by_name("a.gz"), Some("application/gzip"));
        assert_eq!(db.by_name("Makefile"), Some("text/x-makefile"));
        assert_eq!(db.by_name("README.md"), Some("text/x-readme"));
        assert_eq!(db.by_name("main.C"), Some("text/x-c++src"), "регистр важен");
        assert_eq!(db.by_name("main.c"), Some("text/x-csrc"));
        assert_eq!(db.by_name("Отчёт.PNG"), Some("image/png"));
        assert_eq!(db.by_name("noext"), None);
        assert_eq!(db.icon_names("application/gzip"), ["application-gzip", "package-x-generic", "application-x-generic", "text-x-generic"]);
        assert_eq!(db.icon_names(DIRECTORY), ["inode-directory", "folder"]);
    }

    #[test]
    fn glob_and_sniff() {
        assert!(glob_match("*.[ch]", "a.h"));
        assert!(!glob_match("*.[ch]", "a.o"));
        assert!(glob_match("README*", "README"));
        assert!(glob_match("a?c", "abc"));
        let d = std::env::temp_dir().join(format!("conchy-mime-{}", std::process::id()));
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("t"), "привет\n").unwrap();
        fs::write(d.join("b"), [0u8, 1, 2]).unwrap();
        assert_eq!(sniff(&d.join("t")), TEXT);
        assert_eq!(sniff(&d.join("b")), UNKNOWN);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn system_db_if_present() {
        if !Path::new("/usr/share/mime/globs2").exists() {
            return;
        }
        let db = MimeDb::load(&[PathBuf::from("/usr/share")]);
        assert_eq!(db.by_name("x.png"), Some("image/png"));
        assert!(!db.describe("image/png").is_empty());
    }
}
