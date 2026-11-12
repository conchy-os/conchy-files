//! Время чтения одной папки так, как её готовит окно: записи, типы, иконки, сортировка.
//! `cargo run --release --example list_bench -- /usr/lib`

use std::time::Instant;

use conchy_files::{dir, icons, mime};

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let t = Instant::now();
    let db = mime::db();
    let _ = icons::icons();
    let init = t.elapsed();

    let t = Instant::now();
    let mut entries = dir::list(path.as_ref(), true).expect("папка читается");
    let read = t.elapsed();

    let t = Instant::now();
    for e in &entries {
        let names = if e.is_dir { vec!["folder".to_string()] } else { db.icon_names(&e.mime) };
        let _ = icons::icons().lookup(&names, 16);
        let _ = db.describe(&e.mime);
    }
    dir::sort(&mut entries, dir::Sort::default());
    let rest = t.elapsed();

    println!(
        "{path}: {} элементов · база MIME и тема иконок {:.0} мс · чтение {:.0} мс · иконки, описания, сортировка {:.0} мс",
        entries.len(),
        init.as_secs_f64() * 1e3,
        read.as_secs_f64() * 1e3,
        rest.as_secs_f64() * 1e3
    );
}
