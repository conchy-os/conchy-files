//! Что лежит в кеше анализа: `cargo run --release --example cache_info`

use conchy_files::cache::{DiskCache, decode};
use conchy_files::model::{Tree, human_size};

fn main() {
    let dir = DiskCache::default_dir();
    println!("{}", dir.display());
    let Ok(rd) = std::fs::read_dir(&dir) else { return println!("  пусто") };
    for e in rd.flatten() {
        let len = e.metadata().map(|m| m.len()).unwrap_or(0);
        let name = e.file_name().to_string_lossy().into_owned();
        match std::fs::read(e.path()).ok().and_then(|d| decode(&d).ok()) {
            Some(c) => {
                let r = c.tree.node(Tree::ROOT);
                let now = unsafe { libc::time(std::ptr::null_mut()) } as i64;
                println!(
                    "  {name}  {:>7}  {}  ({} файлов, {}, сканировалось {:.1} с, {})",
                    human_size(len),
                    r.name,
                    r.files,
                    human_size(r.size),
                    c.took_ms as f64 / 1000.0,
                    conchy_files::dir::format_date(c.scanned_at, now)
                );
            }
            None => println!("  {name}  {:>7}  не читается", human_size(len)),
        }
    }
}
