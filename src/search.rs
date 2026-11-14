//! Поиск файлов по имени внутри папки и всех вложенных (`Ctrl+F`).
//!
//! Обход идёт в фоне и отдаёт находки пачками, чтобы список наполнялся сразу.
//! На другие файловые системы не переходит, по ссылкам не идёт.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Сколько находок максимум: дальше пользователь всё равно уточнит запрос.
pub const LIMIT: usize = 5000;

/// Запрос: подстрока без учёта регистра или шаблон с `*`/`?`.
pub struct Query {
    text: String,
    glob: bool,
}

impl Query {
    pub fn new(q: &str) -> Self {
        let text = q.trim().to_lowercase();
        Query { glob: text.contains(['*', '?']), text }
    }

    pub fn matches(&self, name: &str) -> bool {
        let n = name.to_lowercase();
        if self.glob { crate::mime::glob_match(&self.text, &n) } else { n.contains(&self.text) }
    }
}

/// Ищет в `root`. `on_batch` получает новые находки не чаще раза в 100 мс;
/// `cancel` прерывает поиск. Возвращает, упёрлись ли в [`LIMIT`].
pub fn find(root: &Path, query: &str, hidden: bool, cancel: &AtomicBool, mut on_batch: impl FnMut(Vec<PathBuf>)) -> bool {
    let q = Query::new(query);
    if q.text.is_empty() {
        return false;
    }
    let Ok(dev) = fs::metadata(root).map(|m| m.dev()) else { return false };
    let mut stack = vec![root.to_path_buf()];
    let mut batch = Vec::new();
    let mut sent = Instant::now();
    let mut total = 0;
    while let Some(dir) = stack.pop() {
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        let mut subdirs = Vec::new();
        for e in rd.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if !hidden && name.starts_with('.') {
                continue;
            }
            let path = e.path();
            if q.matches(&name) {
                batch.push(path.clone());
                total += 1;
                if total >= LIMIT {
                    on_batch(batch);
                    return true;
                }
            }
            if let Ok(m) = e.metadata() {
                if m.is_dir() && m.dev() == dev {
                    subdirs.push(path);
                }
            }
        }
        // Ближние папки раньше дальних: сортируем, чтобы порядок был предсказуемым.
        subdirs.sort();
        stack.extend(subdirs.into_iter().rev());
        if !batch.is_empty() && sent.elapsed() > Duration::from_millis(100) {
            on_batch(std::mem::take(&mut batch));
            sent = Instant::now();
        }
    }
    if !batch.is_empty() {
        on_batch(batch);
    }
    false
}

/// Флаг отмены, общий для фонового поиска и интерфейса.
pub fn cancel_flag() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_recursively() {
        let d = std::env::temp_dir().join(format!("conchy-search-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("a/b")).unwrap();
        fs::create_dir_all(d.join(".hidden")).unwrap();
        for f in ["Отчёт 2024.pdf", "a/отчёт.txt", "a/b/report.md", ".hidden/отчёт.bak", "a/b/other.txt"] {
            fs::write(d.join(f), "").unwrap();
        }
        let run = |q: &str, hidden: bool| {
            let mut all = Vec::new();
            find(&d, q, hidden, &AtomicBool::new(false), |b| all.extend(b));
            let mut names: Vec<String> = all.iter().map(|p| p.strip_prefix(&d).unwrap().display().to_string()).collect();
            names.sort();
            names
        };
        assert_eq!(run("ОТЧЁТ", false), ["a/отчёт.txt", "Отчёт 2024.pdf"]);
        assert_eq!(run("отчёт", true).len(), 3, "скрытые — если включены");
        assert_eq!(run("*.txt", false), ["a/b/other.txt", "a/отчёт.txt"]);
        assert_eq!(run("b", false), ["a/b"], "папки тоже находятся");
        assert!(run("  ", false).is_empty());
        let cancelled = AtomicBool::new(true);
        let mut got = 0;
        find(&d, "о", false, &cancelled, |b| got += b.len());
        assert_eq!(got, 0);
        fs::remove_dir_all(&d).unwrap();
    }
}
