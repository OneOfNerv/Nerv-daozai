//! 屏幕截图与本机暂存。已保存的 PNG 就是历史记录。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[cfg(windows)]
mod selection;

pub const LIMIT: usize = 30;

pub fn directory() -> PathBuf {
    crate::model::config_dir().join("screenshots")
}

fn timestamp(path: &Path) -> Option<i64> {
    let number = path
        .file_name()?
        .to_str()?
        .strip_prefix("shot-")?
        .strip_suffix(".png")?;
    if number.len() != 16 || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    number.parse().ok()
}

pub(crate) fn label(path: &Path) -> String {
    timestamp(path)
        .and_then(chrono::DateTime::from_timestamp_micros)
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format("%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_default()
}

fn list(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if timestamp(&path).is_some() && entry.file_type()?.is_file() {
            paths.push(path);
        }
    }
    paths.sort_unstable_by(|a, b| b.cmp(a));
    Ok(paths)
}

pub(crate) fn history() -> Vec<PathBuf> {
    let mut paths = list(&directory()).unwrap_or_default();
    paths.truncate(LIMIT);
    paths
}

pub(crate) fn remove(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

#[cfg(windows)]
pub enum CaptureOutcome {
    Saved,
    Cancelled,
}

#[cfg(windows)]
pub type CaptureReceiver = std::sync::mpsc::Receiver<Result<CaptureOutcome, String>>;

#[cfg(windows)]
pub fn start(ctx: eframe::egui::Context) -> Result<CaptureReceiver, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("daozai-screenshot".into())
        .spawn(move || {
            let result = selection::capture().and_then(|selected| match selected {
                Some(pixels) => save(&directory(), &pixels)
                    .map(|()| CaptureOutcome::Saved)
                    .map_err(|error| error.to_string()),
                None => Ok(CaptureOutcome::Cancelled),
            });
            let _ = tx.send(result);
            ctx.request_repaint();
        })
        .map_err(|error| error.to_string())?;
    Ok(rx)
}

#[cfg(any(windows, test))]
fn save(dir: &Path, pixels: &image::RgbaImage) -> io::Result<()> {
    use image::ImageEncoder;

    fs::create_dir_all(dir)?;
    let previous = list(dir)?;
    let now = chrono::Utc::now().timestamp_micros();
    let serial = previous
        .first()
        .and_then(|path| timestamp(path))
        .map_or(now, |last| now.max(last + 1));
    let path = dir.join(format!("shot-{serial:016}.png"));
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(
            pixels.as_raw(),
            pixels.width(),
            pixels.height(),
            image::ExtendedColorType::Rgba8,
        )
        .map_err(io::Error::other)?;
    crate::model::atomic_write(&path, &png)?;
    for old in previous.iter().skip(LIMIT - 1) {
        let _ = remove(old);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screenshot_history_reopens_png_and_retires_only_old_captures() {
        let dir = std::env::temp_dir().join(format!(
            "daozai-screenshot-test-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_micros()
        ));
        fs::create_dir(&dir).unwrap();
        let unrelated = dir.join("keep.png");
        fs::write(&unrelated, b"unrelated file").unwrap();
        let pixels = image::RgbaImage::from_pixel(3, 2, image::Rgba([11, 22, 33, 255]));
        for _ in 0..LIMIT {
            save(&dir, &pixels).unwrap();
        }
        let first_run = list(&dir).unwrap();
        assert_eq!(first_run.len(), LIMIT);
        save(&dir, &pixels).unwrap();
        let reopened = list(&dir).unwrap();
        assert_eq!(reopened.len(), LIMIT);
        assert_eq!(&reopened[1..], &first_run[..LIMIT - 1]);
        assert!(!first_run.last().unwrap().exists());
        assert_eq!(image::open(&reopened[0]).unwrap().into_rgba8(), pixels);
        assert!(!label(&reopened[0]).is_empty());
        assert_eq!(fs::read(&unrelated).unwrap(), b"unrelated file");
        remove(&reopened[0]).unwrap();
        assert_eq!(list(&dir).unwrap().len(), LIMIT - 1);
        fs::remove_dir_all(dir).unwrap();
    }
}
