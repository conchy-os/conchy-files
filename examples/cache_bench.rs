//! Кеш анализа против повторного сканирования.
//! `cargo run --release --example cache_bench -- ~/`

use std::time::Instant;

use conchy_files::cache::{DiskCache, encode};
use conchy_files::model::Tree;
use conchy_files::scan::{Progress, ScanOptions, scan};

fn main() {
    let root = std::fs::canonicalize(std::env::args().nth(1).unwrap_or_else(|| ".".into())).expect("папка");
    let t = Instant::now();
    let tree = Tree::from_raw(scan(&root, &ScanOptions::default(), &Progress::default()).expect("сканирование"));
    let scanned = t.elapsed();

    let dir = std::env::temp_dir().join(format!("conchy-cache-bench-{}", std::process::id()));
    let cache = DiskCache::new(dir.clone(), u64::MAX, std::time::Duration::MAX);
    let t = Instant::now();
    cache.save(&tree, 0, scanned.as_millis() as u64, 0).expect("запись");
    let saved = t.elapsed();
    let bytes = encode(&tree, 0, 0, 0).len();

    let t = Instant::now();
    let loaded = cache.load(&root).expect("чтение");
    let load = t.elapsed();
    assert_eq!(loaded.tree.node(Tree::ROOT).size, tree.node(Tree::ROOT).size);

    println!(
        "{}: {} файлов · сканирование {:.2} с · кеш {:.1} МБ, запись {:.0} мс, загрузка {:.0} мс",
        root.display(),
        tree.node(Tree::ROOT).files,
        scanned.as_secs_f64(),
        bytes as f64 / 1048576.0,
        saved.as_secs_f64() * 1e3,
        load.as_secs_f64() * 1e3
    );
    let _ = std::fs::remove_dir_all(dir);
}
