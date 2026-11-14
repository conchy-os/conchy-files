//! Миниатюры картинок по спецификации freedesktop Thumbnail Managing.
//!
//! Кеш общий с Thunar (tumbler), Nautilus и GIMP: `~/.cache/thumbnails/normal`
//! (128 px) и `large` (256 px), имя — MD5 от URI файла. В PNG записаны
//! `Thumb::URI` и `Thumb::MTime`: если файл изменился, миниатюра считается устаревшей.
//! Неудачные попытки помечаются в `fail/conchy-files`, чтобы не повторять их.

use std::fs;
use std::io::{self, BufWriter};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use md5::{Digest, Md5};

/// Картинки больше этого не уменьшаем: долго и много памяти.
const MAX_SOURCE: u64 = 64 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Size {
    Normal,
    Large,
}

impl Size {
    fn px(self) -> u32 {
        match self {
            Size::Normal => 128,
            Size::Large => 256,
        }
    }

    fn dir(self) -> &'static str {
        match self {
            Size::Normal => "normal",
            Size::Large => "large",
        }
    }
}

pub fn cache_dir() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| crate::places::home().join(".cache"))
        .join("thumbnails")
}

/// Типы, которые умеем уменьшать сами.
pub fn supported(mime: &str) -> bool {
    matches!(mime, "image/png" | "image/jpeg" | "image/webp" | "image/gif" | "image/bmp")
}

/// URI файла так же, как у GLib (`g_filename_to_uri`): иначе хеш не совпадёт с чужим кешем.
pub fn uri(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut s = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:@/".contains(&b) {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s
}

fn md5_hex(s: &str) -> String {
    Md5::digest(s.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

pub fn thumb_path(path: &Path, size: Size) -> PathBuf {
    cache_dir().join(size.dir()).join(format!("{}.png", md5_hex(&uri(path))))
}

fn fail_path(path: &Path) -> PathBuf {
    cache_dir().join("fail/conchy-files").join(format!("{}.png", md5_hex(&uri(path))))
}

/// `Thumb::MTime` из PNG без декодирования картинки (текстовые блоки идут до данных).
fn stored_mtime(thumb: &Path) -> Option<i64> {
    let file = fs::File::open(thumb).ok()?;
    let reader = png::Decoder::new(io::BufReader::new(file)).read_info().ok()?;
    let info = reader.info();
    info.uncompressed_latin1_text
        .iter()
        .find(|t| t.keyword == "Thumb::MTime")
        .and_then(|t| t.text.trim().parse().ok())
}

/// Готовая и не устаревшая миниатюра, если есть (своя или сделанная другой программой).
pub fn lookup(path: &Path, size: Size) -> Option<PathBuf> {
    let mtime = fs::metadata(path).ok()?.mtime();
    [size, Size::Large, Size::Normal]
        .into_iter()
        .map(|s| thumb_path(path, s))
        .find(|t| stored_mtime(t) == Some(mtime))
}

/// Были ли уже неудачные попытки для этой версии файла.
pub fn failed_before(path: &Path) -> bool {
    let Ok(mtime) = fs::metadata(path).map(|m| m.mtime()) else { return true };
    stored_mtime(&fail_path(path)) == Some(mtime)
}

/// Делает миниатюру и кладёт в кеш. Медленно — вызывать в фоне.
pub fn generate(path: &Path, size: Size) -> io::Result<PathBuf> {
    let meta = fs::metadata(path)?;
    let res = (|| {
        if meta.len() > MAX_SOURCE {
            return Err(io::Error::other("слишком большой файл"));
        }
        let img = image::ImageReader::open(path)?
            .with_guessed_format()?
            .decode()
            .map_err(io::Error::other)?;
        // Маленькие картинки не увеличиваем: миниатюра — сама картинка.
        let img = if img.width() > size.px() || img.height() > size.px() { img.thumbnail(size.px(), size.px()) } else { img };
        let rgba = img.to_rgba8();
        let out = thumb_path(path, size);
        write_png(&out, rgba.width(), rgba.height(), rgba.as_raw(), path, meta.mtime())?;
        Ok(out)
    })();
    if res.is_err() {
        // Пометка о неудаче — прозрачный PNG 1×1 с теми же метаданными.
        let _ = write_png(&fail_path(path), 1, 1, &[0, 0, 0, 0], path, meta.mtime());
    }
    res
}

fn write_png(out: &Path, w: u32, h: u32, rgba: &[u8], source: &Path, mtime: i64) -> io::Result<()> {
    let dir = out.parent().unwrap();
    fs::create_dir_all(dir)?;
    let tmp = out.with_extension(format!("tmp{}", std::process::id()));
    {
        // По спецификации миниатюры доступны только владельцу.
        let f = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        let mut enc = png::Encoder::new(BufWriter::new(f), w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.add_text_chunk("Thumb::URI".into(), uri(source)).map_err(io::Error::other)?;
        enc.add_text_chunk("Thumb::MTime".into(), mtime.to_string()).map_err(io::Error::other)?;
        enc.add_text_chunk("Software".into(), "conchy-files".into()).map_err(io::Error::other)?;
        let mut writer = enc.write_header().map_err(io::Error::other)?;
        writer.write_image_data(rgba).map_err(io::Error::other)?;
        writer.finish().map_err(io::Error::other)?;
    }
    fs::rename(&tmp, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_and_hash_match_glib() {
        // Проверочное значение из спецификации: file:///home/jens/photos/me.png
        assert_eq!(md5_hex("file:///home/jens/photos/me.png"), "c6ee772d9e49320e97ec29a7eb5b1697");
        assert_eq!(uri(Path::new("/home/u/Мои фото/a b.png")), "file:///home/u/%D0%9C%D0%BE%D0%B8%20%D1%84%D0%BE%D1%82%D0%BE/a%20b.png");
        assert_eq!(uri(Path::new("/x/a+b(1).png")), "file:///x/a+b(1).png");
    }

    #[test]
    fn generates_and_invalidates() {
        crate::test_xdg_cache();
        let d = std::env::temp_dir().join(format!("conchy-thumbs-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        let src = d.join("pic.png");
        image::RgbaImage::from_pixel(400, 200, image::Rgba([200, 30, 30, 255])).save(&src).unwrap();

        assert!(lookup(&src, Size::Normal).is_none());
        let t = generate(&src, Size::Normal).unwrap();
        let (w, h) = image::image_dimensions(&t).unwrap();
        assert_eq!((w, h), (128, 64), "пропорции сохранены");
        assert_eq!(lookup(&src, Size::Normal), Some(t.clone()));
        assert_eq!(fs::metadata(&t).unwrap().mode() & 0o777, 0o600);

        // Файл изменился — миниатюра устарела.
        let f = fs::File::options().write(true).open(&src).unwrap();
        f.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(5)).unwrap();
        assert!(lookup(&src, Size::Normal).is_none());

        // Не картинка — неудача запоминается.
        let bad = d.join("bad.png");
        fs::write(&bad, b"not a png").unwrap();
        assert!(generate(&bad, Size::Normal).is_err());
        assert!(failed_before(&bad));
        fs::remove_dir_all(&d).unwrap();
    }
}
