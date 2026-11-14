//! Сетевые адреса через FUSE, без GLib и gvfs.
//!
//! `sftp://` монтируется `sshfs`, `smb://`, `ftp://`, `dav(s)://` — `rclone mount`.
//! Точка монтирования — `$XDG_RUNTIME_DIR/conchy-files/remote/<адрес>`, после
//! этого ресурс — обычная папка: копирование, корзина, анализ места работают как везде.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scheme {
    Sftp,
    Smb,
    Ftp,
    Webdav { tls: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    pub scheme: Scheme,
    pub user: Option<String>,
    pub host: String,
    pub port: Option<u16>,
    /// Путь на сервере (для SMB первая часть — имя общей папки).
    pub path: String,
}

/// Чем монтировать и почему не вышло.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountError {
    /// Нужной программы нет: что поставить.
    MissingTool(&'static str),
    /// Сервер просит пароль (или текущий не подошёл).
    NeedPassword,
    Failed(String),
}

impl std::fmt::Display for MountError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            MountError::MissingTool(t) => write!(f, "не установлен {t}: поставьте пакет «{t}»"),
            MountError::NeedPassword => write!(f, "нужен пароль"),
            MountError::Failed(e) => write!(f, "{e}"),
        }
    }
}

impl Remote {
    /// `sftp://user@host:2222/home/user/docs` → [`Remote`]. Пароль в адресе не принимаем.
    pub fn parse(url: &str) -> Option<Remote> {
        let (scheme, rest) = url.split_once("://")?;
        let scheme = match scheme.to_ascii_lowercase().as_str() {
            "sftp" | "ssh" => Scheme::Sftp,
            "smb" | "cifs" => Scheme::Smb,
            "ftp" => Scheme::Ftp,
            "dav" | "webdav" => Scheme::Webdav { tls: false },
            "davs" | "webdavs" => Scheme::Webdav { tls: true },
            _ => return None,
        };
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let (user, hostport) = match authority.rsplit_once('@') {
            Some((u, h)) => (Some(String::from_utf8_lossy(&crate::percent_decode(u.split(':').next().unwrap_or(u))).into_owned()), h),
            None => (None, authority),
        };
        let (host, port) = match hostport.rsplit_once(':') {
            Some((h, p)) if !h.is_empty() && p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => (h, p.parse().ok()),
            _ => (hostport, None),
        };
        if host.is_empty() {
            return None;
        }
        let path = String::from_utf8_lossy(&crate::percent_decode(path)).into_owned();
        Some(Remote { scheme, user: user.filter(|u| !u.is_empty()), host: host.to_string(), port, path })
    }

    fn scheme_name(&self) -> &'static str {
        match self.scheme {
            Scheme::Sftp => "sftp",
            Scheme::Smb => "smb",
            Scheme::Ftp => "ftp",
            Scheme::Webdav { tls: false } => "dav",
            Scheme::Webdav { tls: true } => "davs",
        }
    }

    /// Что монтируется целиком: сервер для SFTP/FTP/WebDAV, общая папка для SMB.
    fn share(&self) -> Option<&str> {
        match self.scheme {
            Scheme::Smb => self.path.trim_start_matches('/').split('/').next().filter(|s| !s.is_empty()),
            _ => None,
        }
    }

    /// Имя, под которым ресурс виден в «Сети»: `user@host`, `host/share`.
    pub fn label(&self) -> String {
        let mut s = String::new();
        if let Some(u) = &self.user {
            s.push_str(u);
            s.push('@');
        }
        s.push_str(&self.host);
        if let Some(share) = self.share() {
            s.push('/');
            s.push_str(share);
        }
        s
    }

    pub fn mount_point(&self) -> PathBuf {
        let mut name = format!("{}-{}", self.scheme_name(), self.label().replace('/', "_"));
        if let Some(p) = self.port {
            name.push_str(&format!("-{p}"));
        }
        base_dir().join(name)
    }

    /// Папка внутри точки монтирования, соответствующая адресу.
    pub fn local_path(&self) -> PathBuf {
        let mp = self.mount_point();
        let rel = match self.share() {
            Some(share) => self.path.trim_start_matches('/').strip_prefix(share).unwrap_or("").to_string(),
            None => self.path.clone(),
        };
        let rel = rel.trim_start_matches('/');
        if rel.is_empty() { mp } else { mp.join(rel) }
    }

    /// Команда монтирования. `obscured` — пароль, уже обработанный `rclone obscure`.
    pub fn mount_command(&self, with_password: bool, obscured: Option<&str>) -> Vec<String> {
        let mp = self.mount_point().display().to_string();
        match self.scheme {
            Scheme::Sftp => {
                let target = format!("{}{}:/", self.user.as_ref().map(|u| format!("{u}@")).unwrap_or_default(), self.host);
                let mut opts = "reconnect,ServerAliveInterval=15,ServerAliveCountMax=3,idmap=user".to_string();
                // Без пароля — не задавать вопросов в терминал, которого нет: сразу ошибка.
                opts.push_str(if with_password { ",password_stdin" } else { ",BatchMode=yes" });
                if let Some(p) = self.port {
                    opts.push_str(&format!(",port={p}"));
                }
                vec!["sshfs".into(), target, mp, "-o".into(), opts]
            }
            _ => {
                let (backend, remote_path) = match self.scheme {
                    Scheme::Smb => ("smb", self.share().unwrap_or("").to_string()),
                    Scheme::Ftp => ("ftp", String::new()),
                    _ => ("webdav", String::new()),
                };
                let mut v = vec!["rclone".into(), "mount".into(), format!(":{backend}:{remote_path}"), mp];
                match &self.scheme {
                    Scheme::Webdav { tls } => {
                        let proto = if *tls { "https" } else { "http" };
                        let port = self.port.map(|p| format!(":{p}")).unwrap_or_default();
                        v.extend([format!("--webdav-url={proto}://{}{port}", self.host)]);
                    }
                    _ => {
                        v.push(format!("--{backend}-host={}", self.host));
                        if let Some(p) = self.port {
                            v.push(format!("--{backend}-port={p}"));
                        }
                    }
                }
                if let Some(u) = &self.user {
                    v.push(format!("--{backend}-user={u}"));
                }
                if let Some(p) = obscured {
                    v.push(format!("--{backend}-pass={p}"));
                }
                v.extend(["--daemon".into(), "--vfs-cache-mode=writes".into()]);
                v
            }
        }
    }

    /// Монтирует (если ещё не), возвращает папку, которую открыть. Долго — вызывать в фоне.
    pub fn mount(&self, password: Option<&str>) -> Result<PathBuf, MountError> {
        let mp = self.mount_point();
        if is_mounted(&mp) {
            return Ok(self.local_path());
        }
        let tool = if self.scheme == Scheme::Sftp { "sshfs" } else { "rclone" };
        if which(tool).is_none() {
            return Err(MountError::MissingTool(tool));
        }
        std::fs::create_dir_all(&mp).map_err(|e| MountError::Failed(e.to_string()))?;
        let obscured = match (password, &self.scheme) {
            (Some(p), s) if *s != Scheme::Sftp => Some(obscure(p)?),
            _ => None,
        };
        let argv = self.mount_command(password.is_some(), obscured.as_deref());
        let mut child = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| MountError::Failed(e.to_string()))?;
        if let (Some(p), Scheme::Sftp) = (password, &self.scheme) {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = writeln!(stdin, "{p}");
            }
        }
        let out = child.wait_with_output().map_err(|e| MountError::Failed(e.to_string()))?;
        if out.status.success() && wait_mounted(&mp) {
            return Ok(self.local_path());
        }
        let _ = std::fs::remove_dir(&mp);
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let low = err.to_lowercase();
        if low.contains("permission denied") || low.contains("authentication") || low.contains("password") || low.contains("login") {
            return Err(MountError::NeedPassword);
        }
        Err(MountError::Failed(if err.is_empty() { format!("{tool} завершился с ошибкой") } else { err }))
    }
}

/// Отключает сетевой ресурс.
pub fn unmount(mount_point: &Path) -> Result<(), String> {
    let tool = if which("fusermount3").is_some() { "fusermount3" } else { "fusermount" };
    let out = Command::new(tool).arg("-u").arg(mount_point).output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let _ = std::fs::remove_dir(mount_point);
    Ok(())
}

/// Каталог, куда монтируются сетевые ресурсы.
pub fn base_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join(format!("conchy-files-{}", unsafe { libc::getuid() })))
        .join("conchy-files/remote")
}

/// Подключённые сейчас сетевые ресурсы: точки монтирования внутри [`base_dir`].
pub fn mounted() -> Vec<PathBuf> {
    let base = base_dir();
    let Ok(s) = std::fs::read_to_string("/proc/self/mountinfo") else { return Vec::new() };
    s.lines()
        .filter_map(|l| l.split(' ').nth(4))
        .map(|m| PathBuf::from(m.replace("\\040", " ")))
        .filter(|m| m.parent() == Some(base.as_path()))
        .collect()
}

fn is_mounted(mp: &Path) -> bool {
    mounted().iter().any(|m| m == mp)
}

fn wait_mounted(mp: &Path) -> bool {
    // rclone --daemon возвращается раньше, чем монтирование видно в таблице.
    (0..50).any(|_| {
        if is_mounted(mp) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
        false
    })
}

fn obscure(password: &str) -> Result<String, MountError> {
    // Пароль передаётся через stdin («-»), а не аргументом: так его не видно в списке процессов.
    let mut child = Command::new("rclone")
        .args(["obscure", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| MountError::Failed(e.to_string()))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = write!(stdin, "{password}");
    }
    let out = child.wait_with_output().map_err(|e| MountError::Failed(e.to_string()))?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn which(cmd: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join(cmd)).find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_addresses() {
        let r = Remote::parse("sftp://ivan@server.lan:2222/home/ivan/Мои%20документы").unwrap();
        assert_eq!(r.scheme, Scheme::Sftp);
        assert_eq!((r.user.as_deref(), r.host.as_str(), r.port), (Some("ivan"), "server.lan", Some(2222)));
        assert_eq!(r.path, "/home/ivan/Мои документы");
        assert_eq!(r.label(), "ivan@server.lan");
        assert!(r.mount_point().ends_with("conchy-files/remote/sftp-ivan@server.lan-2222"));
        assert!(r.local_path().ends_with("sftp-ivan@server.lan-2222/home/ivan/Мои документы"));

        let s = Remote::parse("smb://nas/Музыка/Rock").unwrap();
        assert_eq!(s.label(), "nas/Музыка");
        assert!(s.local_path().ends_with("smb-nas_Музыка/Rock"), "{:?}", s.local_path());
        assert_eq!(Remote::parse("davs://cloud.example.org").unwrap().scheme, Scheme::Webdav { tls: true });
        assert_eq!(Remote::parse("sftp://a:secret@h/").unwrap().user.as_deref(), Some("a"), "пароль из адреса не берём");
        assert!(Remote::parse("http://x").is_none() && Remote::parse("sftp:///x").is_none());
    }

    #[test]
    fn builds_mount_commands() {
        let r = Remote::parse("sftp://ivan@server:2222/x").unwrap();
        let cmd = r.mount_command(false, None);
        assert_eq!(cmd[0..2], ["sshfs", "ivan@server:/"]);
        assert!(cmd[4].contains("BatchMode=yes") && cmd[4].contains("port=2222"));
        assert!(r.mount_command(true, None)[4].contains("password_stdin"));

        let s = Remote::parse("smb://user@nas/share/dir").unwrap().mount_command(true, Some("OBSC"));
        assert_eq!(s[0..3], ["rclone", "mount", ":smb:share"]);
        assert!(s.contains(&"--smb-host=nas".into()) && s.contains(&"--smb-user=user".into()) && s.contains(&"--smb-pass=OBSC".into()));
        let w = Remote::parse("davs://cloud:8443/").unwrap().mount_command(false, None);
        assert!(w.contains(&"--webdav-url=https://cloud:8443".into()));
    }
}
