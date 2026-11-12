//! Слежение за изменениями в папках через inotify.
//!
//! Отдельный поток ждёт событий и после короткой паузы (чтобы пачка изменений
//! от копирования дала одно обновление) вызывает `on_change`. Поток
//! останавливается, когда [`Watch`] уничтожается.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub struct Watch {
    stop: Arc<AtomicBool>,
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

const DEBOUNCE: Duration = Duration::from_millis(150);

/// Следит за содержимым папок `dirs` (без вложенных). Вызывает `on_change` после изменений.
pub fn watch(dirs: &[&Path], on_change: impl Fn() + Send + 'static) -> std::io::Result<Watch> {
    let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mask = libc::IN_CREATE
        | libc::IN_DELETE
        | libc::IN_MOVED_FROM
        | libc::IN_MOVED_TO
        | libc::IN_MODIFY
        | libc::IN_ATTRIB
        | libc::IN_CLOSE_WRITE
        | libc::IN_DELETE_SELF
        | libc::IN_MOVE_SELF;
    let mut added = 0;
    for d in dirs {
        let Ok(c) = CString::new(d.as_os_str().as_bytes()) else { continue };
        if unsafe { libc::inotify_add_watch(fd, c.as_ptr(), mask) } >= 0 {
            added += 1;
        }
    }
    if added == 0 {
        let err = std::io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(err);
    }

    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    std::thread::Builder::new()
        .name("conchy-files-watch".into())
        .spawn(move || {
            let mut buf = [0u8; 4096];
            let mut pending: Option<Instant> = None;
            while !flag.load(Ordering::Relaxed) {
                let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
                let r = unsafe { libc::poll(&mut pfd, 1, 100) };
                if r > 0 {
                    // Читаем всё накопившееся; содержимое событий не важно — обновим папку целиком.
                    while unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
                    pending.get_or_insert_with(Instant::now);
                }
                if pending.is_some_and(|t| t.elapsed() >= DEBOUNCE) {
                    pending = None;
                    on_change();
                }
            }
            unsafe { libc::close(fd) };
        })?;
    Ok(Watch { stop })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn reports_changes_once_per_burst() {
        let d = std::env::temp_dir().join(format!("conchy-watch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let w = watch(&[&d], move || {
            h.fetch_add(1, Ordering::Relaxed);
        })
        .unwrap();
        for i in 0..20 {
            std::fs::write(d.join(format!("f{i}")), "x").unwrap();
        }
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(hits.load(Ordering::Relaxed), 1, "пачка изменений — одно обновление");
        drop(w);
        std::thread::sleep(Duration::from_millis(200));
        std::fs::write(d.join("after"), "x").unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(hits.load(Ordering::Relaxed), 1, "после остановки событий нет");
        std::fs::remove_dir_all(&d).unwrap();
    }
}
