//! Программы для открытия файлов: `.desktop`-файлы и `mimeapps.list` по
//! спецификациям freedesktop (Desktop Entry, MIME Applications Associations).
//!
//! Те же данные, что у Thunar, Nautilus и `xdg-mime`: программа по умолчанию,
//! выбранная здесь, станет ею и для других программ.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct App {
    /// Идентификатор: имя файла (`org.gnome.Loupe.desktop`; из подпапок — через `-`).
    pub id: String,
    pub name: String,
    pub exec: String,
    pub icon: String,
    pub terminal: bool,
    pub mime_types: Vec<String>,
    pub path: PathBuf,
}

fn data_dirs() -> Vec<PathBuf> {
    let mut v = Vec::new();
    let home = crate::places::home();
    v.push(std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).filter(|p| p.is_absolute()).unwrap_or_else(|| home.join(".local/share")));
    let sys = std::env::var("XDG_DATA_DIRS").unwrap_or_default();
    let sys = if sys.is_empty() { "/usr/local/share:/usr/share".to_string() } else { sys };
    v.extend(sys.split(':').filter(|s| !s.is_empty()).map(PathBuf::from));
    v
}

fn config_dirs() -> Vec<PathBuf> {
    let home = crate::places::home();
    let mut v = vec![std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).filter(|p| p.is_absolute()).unwrap_or_else(|| home.join(".config"))];
    let sys = std::env::var("XDG_CONFIG_DIRS").unwrap_or_default();
    let sys = if sys.is_empty() { "/etc/xdg".to_string() } else { sys };
    v.extend(sys.split(':').filter(|s| !s.is_empty()).map(PathBuf::from));
    v
}

/// Язык для `Name[ru]`: из `LC_ALL`/`LC_MESSAGES`/`LANG`.
fn lang() -> Option<String> {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .filter_map(|k| std::env::var(k).ok())
        .find(|v| !v.is_empty() && v != "C" && v != "POSIX")
        .map(|v| v.split(['.', '@']).next().unwrap_or("").to_string())
}

/// Разбирает `.desktop`. `None` — не приложение, скрыто или не запускается.
pub fn parse_desktop(text: &str, id: &str, path: &Path, lang: Option<&str>) -> Option<App> {
    let mut in_entry = false;
    let mut kv: HashMap<&str, &str> = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            kv.entry(k.trim()).or_insert(v.trim());
        }
    }
    if kv.get("Type").copied() != Some("Application") || kv.get("Hidden") == Some(&"true") {
        return None;
    }
    let exec = kv.get("Exec")?.to_string();
    if let Some(try_exec) = kv.get("TryExec") {
        if find_in_path(try_exec).is_none() {
            return None;
        }
    }
    // Name[ru_RU] → Name[ru] → Name.
    let name = lang
        .and_then(|l| kv.get(format!("Name[{l}]").as_str()).or_else(|| kv.get(format!("Name[{}]", l.split('_').next().unwrap_or(l)).as_str())))
        .or_else(|| kv.get("Name"))?
        .to_string();
    Some(App {
        id: id.to_string(),
        name,
        exec,
        icon: kv.get("Icon").unwrap_or(&"").to_string(),
        terminal: kv.get("Terminal") == Some(&"true"),
        mime_types: kv.get("MimeType").map(|m| m.split(';').filter(|s| !s.is_empty()).map(String::from).collect()).unwrap_or_default(),
        path: path.to_path_buf(),
    })
    // NoDisplay=true программы не прячем совсем: в «Открыть с помощью» они нужны (как у GIO).
}

fn find_in_path(cmd: &str) -> Option<PathBuf> {
    if cmd.contains('/') {
        return Path::new(cmd).is_file().then(|| PathBuf::from(cmd));
    }
    std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join(cmd)).find(|p| p.is_file())
}

/// Все установленные приложения. Файл из каталога выше по приоритету перекрывает тот же id ниже.
pub fn all_apps() -> Vec<App> {
    let lang = lang();
    let mut seen: HashMap<String, App> = HashMap::new();
    for dir in data_dirs() {
        let base = dir.join("applications");
        let mut stack = vec![base.clone()];
        while let Some(d) = stack.pop() {
            let Ok(rd) = fs::read_dir(&d) else { continue };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                    continue;
                }
                if p.extension().and_then(|x| x.to_str()) != Some("desktop") {
                    continue;
                }
                // Подпапки дают id через дефис: kde/org.kde.okular.desktop → kde-org.kde.okular.desktop.
                let id = p.strip_prefix(&base).map(|r| r.to_string_lossy().replace('/', "-")).unwrap_or_default();
                if seen.contains_key(&id) {
                    continue;
                }
                if let Some(app) = fs::read_to_string(&p).ok().and_then(|t| parse_desktop(&t, &id, &p, lang.as_deref())) {
                    seen.insert(id, app);
                }
            }
        }
    }
    let mut v: Vec<App> = seen.into_values().collect();
    v.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    v
}

/// Содержимое `mimeapps.list`: (по умолчанию, добавленные, удалённые) для каждого типа.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Associations {
    pub defaults: HashMap<String, Vec<String>>,
    pub added: HashMap<String, Vec<String>>,
    pub removed: HashMap<String, Vec<String>>,
}

pub fn parse_mimeapps(text: &str, into: &mut Associations) {
    let mut section = "";
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            section = line;
            continue;
        }
        let Some((mime, ids)) = line.split_once('=') else { continue };
        let ids: Vec<String> = ids.split(';').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect();
        let map = match section {
            "[Default Applications]" => &mut into.defaults,
            "[Added Associations]" => &mut into.added,
            "[Removed Associations]" => &mut into.removed,
            _ => continue,
        };
        // Первый найденный файл важнее: не перезаписываем.
        map.entry(mime.trim().to_string()).or_insert(ids);
    }
}

/// `mimeapps.list` по порядку важности: `~/.config`, `/etc/xdg`, `…/applications`.
pub fn associations() -> Associations {
    let mut a = Associations::default();
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().to_lowercase();
    let mut files = Vec::new();
    for d in config_dirs() {
        for de in desktop.split(':').filter(|s| !s.is_empty()) {
            files.push(d.join(format!("{de}-mimeapps.list")));
        }
        files.push(d.join("mimeapps.list"));
    }
    for d in data_dirs() {
        files.push(d.join("applications/mimeapps.list"));
        files.push(d.join("applications/defaults.list"));
    }
    for f in files {
        if let Ok(t) = fs::read_to_string(&f) {
            parse_mimeapps(&t, &mut a);
        }
    }
    a
}

/// Программы для типа: по умолчанию первой, затем добавленные пользователем, затем
/// объявившие этот тип сами. Для `text/*` подходят и программы для `text/plain`.
pub fn apps_for(mime: &str, all: &[App], assoc: &Associations) -> Vec<App> {
    let mut mimes = vec![mime.to_string()];
    if mime.starts_with("text/") && mime != "text/plain" {
        mimes.push("text/plain".into());
    }
    let removed: Vec<&String> = mimes.iter().filter_map(|m| assoc.removed.get(m)).flatten().collect();
    let by_id = |id: &str| all.iter().find(|a| a.id == id);
    let mut out: Vec<App> = Vec::new();
    let push = |a: &App, out: &mut Vec<App>| {
        if !out.iter().any(|x| x.id == a.id) && !removed.contains(&&a.id) {
            out.push(a.clone());
        }
    };
    for m in &mimes {
        for id in assoc.defaults.get(m).into_iter().flatten() {
            if let Some(a) = by_id(id) {
                push(a, &mut out);
                break;
            }
        }
    }
    for m in &mimes {
        for id in assoc.added.get(m).into_iter().flatten() {
            if let Some(a) = by_id(id) {
                push(a, &mut out);
            }
        }
    }
    // Без явного выбора пользователя графические программы — раньше консольных:
    // двойной щелчок не должен неожиданно открывать редактор в терминале.
    let explicit = out.len();
    for m in &mimes {
        for a in all.iter().filter(|a| a.mime_types.iter().any(|t| t == m)) {
            push(a, &mut out);
        }
    }
    out[explicit..].sort_by_key(|a| a.terminal);
    out
}

/// Делает программу умолчанием для типа: правит `[Default Applications]` в
/// `~/.config/mimeapps.list`, остальное в файле не трогает (как `xdg-mime default`).
pub fn set_default(mime: &str, app_id: &str) -> std::io::Result<()> {
    let path = config_dirs()[0].join("mimeapps.list");
    let text = fs::read_to_string(&path).unwrap_or_default();
    fs::create_dir_all(path.parent().unwrap())?;
    fs::write(&path, with_default(&text, mime, app_id))
}

fn with_default(text: &str, mime: &str, app_id: &str) -> String {
    let mut out = Vec::new();
    let mut in_defaults = false;
    let mut done = false;
    let line = format!("{mime}={app_id};");
    for l in text.lines() {
        let t = l.trim();
        if t.starts_with('[') {
            if in_defaults && !done {
                out.push(line.clone());
                done = true;
            }
            in_defaults = t == "[Default Applications]";
        } else if in_defaults && t.split_once('=').is_some_and(|(k, _)| k.trim() == mime) {
            if !done {
                out.push(line.clone());
                done = true;
            }
            continue;
        }
        out.push(l.to_string());
    }
    if !done {
        if !in_defaults {
            if out.last().is_some_and(|l| !l.is_empty()) {
                out.push(String::new());
            }
            out.push("[Default Applications]".into());
        }
        out.push(line);
    }
    out.join("\n") + "\n"
}

/// Разбирает `Exec` по правилам спецификации (кавычки, экранирование) и подставляет
/// коды полей. Возвращает команды: при `%f`/`%u` — по одной на файл.
pub fn expand_exec(app: &App, files: &[PathBuf]) -> Vec<Vec<String>> {
    let words = split_exec(&app.exec);
    let single = words.iter().any(|w| w.contains("%f") || w.contains("%u"));
    let uri = |p: &Path| format!("file://{}", encode_path(p));
    let build = |chunk: &[PathBuf]| -> Vec<String> {
        let mut argv = Vec::new();
        for w in &words {
            match w.as_str() {
                "%F" => argv.extend(chunk.iter().map(|p| p.display().to_string())),
                "%U" => argv.extend(chunk.iter().map(|p| uri(p))),
                "%i" if !app.icon.is_empty() => {
                    argv.push("--icon".into());
                    argv.push(app.icon.clone());
                }
                "%i" => {}
                _ => {
                    let mut s = String::new();
                    let mut chars = w.chars().peekable();
                    while let Some(c) = chars.next() {
                        if c != '%' {
                            s.push(c);
                            continue;
                        }
                        match chars.next() {
                            Some('%') => s.push('%'),
                            Some('f') => s.push_str(&chunk.first().map(|p| p.display().to_string()).unwrap_or_default()),
                            Some('u') => s.push_str(&chunk.first().map(|p| uri(p)).unwrap_or_default()),
                            Some('c') => s.push_str(&app.name),
                            Some('k') => s.push_str(&app.path.display().to_string()),
                            _ => {} // %d %D %n %N %v %m — устаревшие, убираются
                        }
                    }
                    if !s.is_empty() || !w.contains('%') {
                        argv.push(s);
                    }
                }
            }
        }
        argv
    };
    if single && files.len() > 1 { files.iter().map(|f| build(std::slice::from_ref(f))).collect() } else { vec![build(files)] }
}

fn split_exec(exec: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = exec.chars();
    let mut have = false;
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                have = true;
            }
            '\\' if quoted => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            ' ' | '\t' if !quoted => {
                if have || !cur.is_empty() {
                    words.push(std::mem::take(&mut cur));
                    have = false;
                }
            }
            _ => cur.push(c),
        }
    }
    if have || !cur.is_empty() {
        words.push(cur);
    }
    words
}

fn encode_path(p: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    p.as_os_str()
        .as_bytes()
        .iter()
        .map(|&b| if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

/// Запускает программу с файлами. Процесс отвязывается от нас и живёт сам.
/// `terminal` — чем запускать программы с `Terminal=true`.
pub fn launch(app: &App, files: &[PathBuf], terminal: Option<&str>, cwd: Option<&Path>) -> std::io::Result<()> {
    for mut argv in expand_exec(app, files) {
        if argv.is_empty() {
            continue;
        }
        if app.terminal {
            let t = terminal.ok_or_else(|| std::io::Error::other("не найден терминал для консольной программы"))?;
            argv.splice(0..0, [t.to_string(), "-e".into()]);
        }
        let mut cmd = std::process::Command::new(&argv[0]);
        cmd.args(&argv[1..]).stdin(std::process::Stdio::null());
        if let Some(d) = cwd {
            cmd.current_dir(d);
        }
        let mut child = cmd.spawn()?;
        // Забираем код выхода в фоне, чтобы не копить зомби.
        std::thread::spawn(move || child.wait());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOUPE: &str = "[Desktop Entry]\nType=Application\nName=Image Viewer\nName[ru]=Просмотр изображений\nExec=loupe %U\nIcon=org.gnome.Loupe\nMimeType=image/png;image/jpeg;\n\n[Desktop Action new]\nName=Other\nExec=bad\n";

    #[test]
    fn parses_desktop_files() {
        let a = parse_desktop(LOUPE, "loupe.desktop", Path::new("/x"), Some("ru_RU")).unwrap();
        assert_eq!(a.name, "Просмотр изображений");
        assert_eq!(a.exec, "loupe %U", "Exec из раздела действия не перекрывает основной");
        assert_eq!(a.mime_types, ["image/png", "image/jpeg"]);
        assert!(parse_desktop("[Desktop Entry]\nType=Link\nName=x\nURL=y", "l", Path::new("/x"), None).is_none());
        assert!(parse_desktop("[Desktop Entry]\nType=Application\nName=x\nExec=x\nHidden=true", "h", Path::new("/x"), None).is_none());
        assert!(parse_desktop("[Desktop Entry]\nType=Application\nName=x\nExec=x\nTryExec=no-such-binary-xyz", "t", Path::new("/x"), None).is_none());
    }

    #[test]
    fn expands_exec_field_codes() {
        let mut a = parse_desktop(LOUPE, "loupe.desktop", Path::new("/usr/share/applications/loupe.desktop"), None).unwrap();
        let files = [PathBuf::from("/tmp/a b.png"), PathBuf::from("/tmp/Фото.jpg")];
        assert_eq!(expand_exec(&a, &files), vec![vec!["loupe", "file:///tmp/a%20b.png", "file:///tmp/%D0%A4%D0%BE%D1%82%D0%BE.jpg"]]);
        a.exec = r#"sh -c "echo \"%f\"" %f --name=%c %i 100%%"#.into();
        let cmds = expand_exec(&a, &files);
        assert_eq!(cmds.len(), 2, "%f — по команде на файл");
        assert_eq!(cmds[0], ["sh", "-c", "echo \"/tmp/a b.png\"", "/tmp/a b.png", "--name=Image Viewer", "--icon", "org.gnome.Loupe", "100%"]);
        a.exec = "app %F %d".into();
        assert_eq!(expand_exec(&a, &files[..1]), vec![vec!["app", "/tmp/a b.png"]]);
    }

    #[test]
    fn associations_order_and_edit() {
        let all = vec![
            parse_desktop(LOUPE, "loupe.desktop", Path::new("/x"), None).unwrap(),
            parse_desktop("[Desktop Entry]\nType=Application\nName=GIMP\nExec=gimp %U\nMimeType=image/png;", "gimp.desktop", Path::new("/x"), None).unwrap(),
            parse_desktop("[Desktop Entry]\nType=Application\nName=Ed\nExec=ed %F\nMimeType=text/plain;", "ed.desktop", Path::new("/x"), None).unwrap(),
            parse_desktop("[Desktop Entry]\nType=Application\nName=Atom\nExec=atom %F\nTerminal=true\nMimeType=text/plain;", "atom.desktop", Path::new("/x"), None).unwrap(),
            parse_desktop("[Desktop Entry]\nType=Application\nName=Zed\nExec=zed %U\nMimeType=text/plain;", "zed.desktop", Path::new("/x"), None).unwrap(),
        ];
        let mut assoc = Associations::default();
        parse_mimeapps("[Default Applications]\nimage/png=gimp.desktop;\n[Removed Associations]\ntext/x-rust=ed.desktop;\n", &mut assoc);
        let ids = |m: &str| apps_for(m, &all, &assoc).into_iter().map(|a| a.id).collect::<Vec<_>>();
        assert_eq!(ids("image/png"), ["gimp.desktop", "loupe.desktop"], "умолчание первым");
        assert_eq!(ids("text/x-csrc"), ["ed.desktop", "zed.desktop", "atom.desktop"], "для text/* — программы text/plain, консольные последними");
        let mut chosen = assoc.clone();
        parse_mimeapps("[Default Applications]\ntext/plain=atom.desktop;\n", &mut chosen);
        assert_eq!(apps_for("text/plain", &all, &chosen)[0].id, "atom.desktop", "явный выбор пользователя главнее");
        assert!(!ids("text/x-rust").contains(&"ed.desktop".to_string()), "убранная пользователем связь не предлагается");

        let edited = with_default("# мой файл\n[Added Associations]\nx/y=a;\n[Default Applications]\nimage/png=gimp.desktop;\ntext/plain=ed.desktop;\n", "image/png", "loupe.desktop");
        assert_eq!(edited, "# мой файл\n[Added Associations]\nx/y=a;\n[Default Applications]\nimage/png=loupe.desktop;\ntext/plain=ed.desktop;\n");
        assert_eq!(with_default("", "a/b", "x.desktop"), "[Default Applications]\na/b=x.desktop;\n");
    }
}
