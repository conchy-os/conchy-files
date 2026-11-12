//! Поиск иконок по спецификации freedesktop Icon Theme.
//!
//! Тема берётся из `~/.config/gtk-3.0/settings.ini` (`gtk-icon-theme-name`),
//! как у GTK-программ, иначе Adwaita. Цепочка наследования (`Inherits`)
//! заканчивается темой hicolor. Найденные пути кешируются.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

#[derive(Debug, Clone)]
struct SubDir {
    path: String,
    min: u32,
    max: u32,
    scalable: bool,
}

#[derive(Debug, Clone)]
struct Theme {
    /// Каталоги темы во всех базовых путях (`~/.local/share/icons/X`, `/usr/share/icons/X`).
    roots: Vec<PathBuf>,
    dirs: Vec<SubDir>,
}

#[derive(Debug, Default)]
pub struct Icons {
    chain: Vec<Theme>,
    /// Иконки вне тем (`/usr/share/pixmaps`).
    fallback: Vec<PathBuf>,
    cache: Mutex<HashMap<(String, u32), Option<PathBuf>>>,
}

/// Тема пользователя. Загружается один раз.
pub fn icons() -> &'static Icons {
    static I: OnceLock<Icons> = OnceLock::new();
    I.get_or_init(|| Icons::load(&user_theme_name(), &base_dirs()))
}

fn base_dirs() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        v.push(home.join(".icons"));
        let data = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| home.join(".local/share"));
        v.push(data.join("icons"));
    }
    let sys = std::env::var("XDG_DATA_DIRS").unwrap_or_default();
    let sys = if sys.is_empty() { "/usr/local/share:/usr/share".to_string() } else { sys };
    v.extend(sys.split(':').filter(|s| !s.is_empty()).map(|d| PathBuf::from(d).join("icons")));
    v
}

fn user_theme_name() -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let conf = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".config"));
    for f in ["gtk-4.0/settings.ini", "gtk-3.0/settings.ini"] {
        if let Ok(s) = fs::read_to_string(conf.join(f)) {
            for line in s.lines() {
                if let Some((k, v)) = line.split_once('=') {
                    if k.trim() == "gtk-icon-theme-name" {
                        return v.trim().trim_matches('"').to_string();
                    }
                }
            }
        }
    }
    "Adwaita".into()
}

impl Icons {
    pub fn load(theme: &str, bases: &[PathBuf]) -> Self {
        let mut chain = Vec::new();
        let mut queue = vec![theme.to_string()];
        while let Some(name) = queue.first().cloned() {
            queue.remove(0);
            if chain.iter().any(|(n, _): &(String, Theme)| *n == name) {
                continue;
            }
            let Some((theme, inherits)) = load_theme(&name, bases) else { continue };
            chain.push((name, theme));
            queue.extend(inherits);
        }
        if !chain.iter().any(|(n, _)| n == "hicolor") {
            if let Some((t, _)) = load_theme("hicolor", bases) {
                chain.push(("hicolor".into(), t));
            }
        }
        let fallback = bases.iter().filter_map(|b| b.parent().map(|p| p.join("pixmaps"))).collect();
        Icons { chain: chain.into_iter().map(|(_, t)| t).collect(), fallback, cache: Mutex::default() }
    }

    /// Первая найденная иконка из списка имён (от точного к общему) для размера `size`.
    pub fn lookup(&self, names: &[impl AsRef<str>], size: u32) -> Option<PathBuf> {
        let key = (names.iter().map(|n| n.as_ref()).collect::<Vec<_>>().join("|"), size);
        if let Some(hit) = self.cache.lock().unwrap().get(&key) {
            return hit.clone();
        }
        let found = self.find(names, size);
        self.cache.lock().unwrap().insert(key, found.clone());
        found
    }

    fn find(&self, names: &[impl AsRef<str>], size: u32) -> Option<PathBuf> {
        // По спецификации: сначала все имена в теме, потом в родителях.
        for theme in &self.chain {
            for name in names {
                if let Some(p) = find_in_theme(theme, name.as_ref(), size) {
                    return Some(p);
                }
            }
        }
        for name in names {
            for dir in &self.fallback {
                for ext in ["svg", "png"] {
                    let p = dir.join(format!("{}.{ext}", name.as_ref()));
                    if p.is_file() {
                        return Some(p);
                    }
                }
            }
        }
        None
    }
}

fn load_theme(name: &str, bases: &[PathBuf]) -> Option<(Theme, Vec<String>)> {
    let roots: Vec<PathBuf> = bases.iter().map(|b| b.join(name)).filter(|p| p.is_dir()).collect();
    let index = roots.iter().find_map(|r| fs::read_to_string(r.join("index.theme")).ok())?;
    let mut inherits = Vec::new();
    let mut listed = Vec::new();
    let mut sections: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut cur = String::new();
    for line in index.lines().map(str::trim) {
        if let Some(s) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            cur = s.to_string();
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        let (k, v) = (k.trim(), v.trim());
        if cur == "Icon Theme" {
            match k {
                "Inherits" => inherits = v.split(',').map(|s| s.trim().to_string()).collect(),
                "Directories" | "ScaledDirectories" => listed.extend(v.split(',').map(|s| s.trim().to_string())),
                _ => {}
            }
        } else {
            sections.entry(cur.clone()).or_default().insert(k.to_string(), v.to_string());
        }
    }
    let dirs = listed
        .into_iter()
        .filter(|d| !d.is_empty())
        .filter_map(|d| {
            let s = sections.get(&d)?;
            if s.get("Scale").is_some_and(|v| v != "1") {
                return None;
            }
            let size: u32 = s.get("Size")?.parse().ok()?;
            let kind = s.get("Type").map(String::as_str).unwrap_or("Threshold");
            let num = |k: &str, d: u32| s.get(k).and_then(|v| v.parse().ok()).unwrap_or(d);
            let (min, max) = match kind {
                "Fixed" => (size, size),
                "Scalable" => (num("MinSize", size), num("MaxSize", size)),
                _ => {
                    let t = num("Threshold", 2);
                    (size.saturating_sub(t), size + t)
                }
            };
            Some(SubDir { scalable: kind == "Scalable", path: d, min, max })
        })
        .collect();
    Some((Theme { roots, dirs }, inherits))
}

fn find_in_theme(theme: &Theme, name: &str, size: u32) -> Option<PathBuf> {
    let mut best: Option<(u32, PathBuf)> = None;
    for d in &theme.dirs {
        for root in &theme.roots {
            for ext in ["svg", "png"] {
                let p = root.join(&d.path).join(format!("{name}.{ext}"));
                if !p.is_file() {
                    continue;
                }
                if (d.min..=d.max).contains(&size) {
                    return Some(p);
                }
                let dist = if size < d.min { d.min - size } else { size.saturating_sub(d.max) };
                // Вектор масштабируется без потерь: при равном расстоянии он лучше.
                let dist = if d.scalable { dist / 2 } else { dist };
                if best.as_ref().is_none_or(|(b, _)| dist < *b) {
                    best = Some((dist, p));
                }
            }
        }
    }
    best.map(|(_, p)| p)
}

/// Путь к символьной (одноцветной) иконке для кнопок: `go-previous` → `.../go-previous-symbolic.svg`.
pub fn symbolic(name: &str) -> Option<PathBuf> {
    icons().lookup(&[format!("{name}-symbolic")], 16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_in_theme_and_parent() {
        let base = std::env::temp_dir().join(format!("conchy-icons-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let mk = |theme: &str, index: &str, files: &[&str]| {
            let root = base.join(theme);
            fs::create_dir_all(&root).unwrap();
            fs::write(root.join("index.theme"), index).unwrap();
            for f in files {
                let p = root.join(f);
                fs::create_dir_all(p.parent().unwrap()).unwrap();
                fs::write(p, "<svg/>").unwrap();
            }
        };
        mk(
            "Child",
            "[Icon Theme]\nInherits=Parent\nDirectories=16x16/places,scalable/places\n\
             [16x16/places]\nSize=16\nType=Fixed\n[scalable/places]\nSize=128\nMinSize=8\nMaxSize=512\nType=Scalable\n",
            &["scalable/places/folder.svg", "16x16/places/user-home.svg"],
        );
        mk("Parent", "[Icon Theme]\nDirectories=48x48/mimetypes\n[48x48/mimetypes]\nSize=48\nType=Threshold\n", &[
            "48x48/mimetypes/image-png.svg",
        ]);
        let icons = Icons::load("Child", &[base.clone()]);
        let f = icons.lookup(&["folder"], 16).unwrap();
        assert!(f.ends_with("Child/scalable/places/folder.svg"));
        assert!(icons.lookup(&["user-home"], 16).unwrap().ends_with("16x16/places/user-home.svg"));
        assert!(icons.lookup(&["nope", "image-png"], 16).unwrap().ends_with("Parent/48x48/mimetypes/image-png.svg"));
        assert_eq!(icons.lookup(&["nope"], 16), None);
        fs::remove_dir_all(&base).unwrap();
    }
}
