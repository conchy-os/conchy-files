//! Одиночный экземпляр: первый запуск становится основным процессом и слушает
//! сокет `$XDG_RUNTIME_DIR/conchy-files.sock`; следующие запуски передают ему, что
//! открыть, и сразу выходят. Новое окно открывается мгновенно, без второго процесса.
//!
//! Запросы — записи, разделённые нулевым байтом (в путях бывает что угодно, кроме `\0`):
//! `open\0/путь\0`, `select\0/путь\0`, `props\0/путь\0`.

use std::io::{self, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Что попросили сделать.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Открыть папку в новом окне (для файла — его папку с выделенным файлом).
    Open(PathBuf),
    /// Открыть папку элемента и выделить его («Показать в папке»).
    Select(PathBuf),
    /// То же и открыть нижнюю панель со свойствами.
    Properties(PathBuf),
}

impl Request {
    pub fn path(&self) -> &Path {
        match self {
            Request::Open(p) | Request::Select(p) | Request::Properties(p) => p,
        }
    }
}

pub fn socket_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join(format!("conchy-files-{}", unsafe { libc::getuid() })));
    dir.join("conchy-files.sock")
}

pub fn encode(reqs: &[Request]) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    let mut out = Vec::new();
    for r in reqs {
        let kind: &[u8] = match r {
            Request::Open(_) => b"open",
            Request::Select(_) => b"select",
            Request::Properties(_) => b"props",
        };
        out.extend_from_slice(kind);
        out.push(0);
        out.extend_from_slice(r.path().as_os_str().as_bytes());
        out.push(0);
    }
    out
}

pub fn decode(data: &[u8]) -> Vec<Request> {
    use std::os::unix::ffi::OsStrExt;
    let mut parts = data.split(|&b| b == 0);
    let mut out = Vec::new();
    while let (Some(kind), Some(path)) = (parts.next(), parts.next()) {
        let p = PathBuf::from(std::ffi::OsStr::from_bytes(path));
        match kind {
            b"open" => out.push(Request::Open(p)),
            b"select" => out.push(Request::Select(p)),
            b"props" => out.push(Request::Properties(p)),
            _ => {}
        }
    }
    out
}

/// Пытается передать запросы работающему экземпляру. `Ok(true)` — передано, можно выходить.
pub fn send_to_running(socket: &Path, reqs: &[Request]) -> io::Result<bool> {
    let mut s = match UnixStream::connect(socket) {
        Ok(s) => s,
        Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused) => return Ok(false),
        Err(e) => return Err(e),
    };
    s.set_write_timeout(Some(Duration::from_secs(2)))?;
    s.write_all(&encode(reqs))?;
    s.shutdown(std::net::Shutdown::Write)?;
    // Ждём подтверждения: основной процесс мог зависнуть — тогда запускаемся сами.
    s.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut ok = [0u8; 2];
    Ok(s.read_exact(&mut ok).is_ok() && &ok == b"ok")
}

/// Становится основным экземпляром. Устаревший сокет от упавшего процесса удаляется.
pub fn listen(socket: &Path) -> io::Result<UnixListener> {
    match UnixListener::bind(socket) {
        Ok(l) => Ok(l),
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
            if UnixStream::connect(socket).is_ok() {
                return Err(e); // кто-то живой уже слушает
            }
            std::fs::remove_file(socket)?;
            UnixListener::bind(socket)
        }
        Err(e) => Err(e),
    }
}

/// Принимает подключения в отдельном потоке и отдаёт запросы в `on_request`.
pub fn serve(listener: UnixListener, on_request: impl Fn(Vec<Request>) + Send + 'static) {
    std::thread::Builder::new()
        .name("conchy-files-instance".into())
        .spawn(move || {
            for conn in listener.incoming().flatten() {
                let mut conn = conn;
                let _ = conn.set_read_timeout(Some(Duration::from_secs(2)));
                let mut buf = Vec::new();
                if conn.read_to_end(&mut buf).is_ok() {
                    let _ = conn.write_all(b"ok");
                    on_request(decode(&buf));
                }
            }
        })
        .ok();
}

/// Аргумент командной строки или URI (`file:///…`) → путь.
pub fn arg_to_path(arg: &str) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    match arg.strip_prefix("file://") {
        Some(rest) => {
            // file://host/path — хост пропускаем.
            let rest = if rest.starts_with('/') { rest } else { &rest[rest.find('/')?..] };
            Some(PathBuf::from(std::ffi::OsString::from_vec(crate::percent_decode(rest))))
        }
        None if arg.contains("://") => None,
        None => Some(PathBuf::from(arg)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_and_decodes() {
        let reqs = vec![
            Request::Open("/home/u/Мои файлы".into()),
            Request::Select("/tmp/a\nb".into()),
            Request::Properties("/".into()),
        ];
        assert_eq!(decode(&encode(&reqs)), reqs);
        assert_eq!(decode(b"junk\0/x\0open\0/y\0"), vec![Request::Open("/y".into())]);
        assert_eq!(arg_to_path("file:///home/u/%D0%A4%20x"), Some("/home/u/Ф x".into()));
        assert_eq!(arg_to_path("file://localhost/etc"), Some("/etc".into()));
        assert_eq!(arg_to_path("sftp://host/x"), None);
        assert_eq!(arg_to_path("relative"), Some("relative".into()));
    }

    #[test]
    fn second_instance_hands_over() {
        let dir = std::env::temp_dir().join(format!("conchy-inst-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("s.sock");
        assert!(!send_to_running(&sock, &[Request::Open("/".into())]).unwrap(), "никого нет");

        // Устаревший файл сокета от упавшего процесса не мешает.
        drop(UnixListener::bind(&sock).unwrap());
        let listener = listen(&sock).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        serve(listener, move |r| tx.send(r).unwrap());
        assert!(send_to_running(&sock, &[Request::Select("/etc/hosts".into())]).unwrap());
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), vec![Request::Select("/etc/hosts".into())]);
        assert!(listen(&sock).is_err(), "второй основной не нужен");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
