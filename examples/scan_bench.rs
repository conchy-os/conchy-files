//! Замер скорости сканера без интерфейса: `cargo run --release --example scan_bench -- ~/`
use std::sync::atomic::Ordering;
use std::time::Instant;

use conchy_files::model::{Tree, human_size};
use conchy_files::scan::{Progress, ScanOptions, scan};

fn main() -> anyhow::Result<()> {
    let root = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let p = Progress::default();
    let t = Instant::now();
    let raw = scan(root.as_ref(), &ScanOptions::default(), &p)?;
    let scanned = t.elapsed();
    let tree = Tree::from_raw(raw);
    let total = t.elapsed();
    let r = tree.node(Tree::ROOT);
    println!(
        "{} файлов, {} папок, {} (на диске {}), ошибок {}\nобход {:.2} с, всего {:.2} с",
        p.files.load(Ordering::Relaxed),
        p.dirs.load(Ordering::Relaxed),
        human_size(r.size),
        human_size(r.alloc),
        p.errors.load(Ordering::Relaxed),
        scanned.as_secs_f32(),
        total.as_secs_f32()
    );
    Ok(())
}
