//! Снимки экрана: для модели со зрением и для раздачи помощнику.

use anyhow::{Context, Result};
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::{ExtendedColorType, ImageEncoder, RgbaImage};

pub struct Shot {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// Что именно попало в кадр — заголовок окна или «весь экран».
    pub source: String,
}

/// Что снимать для модели.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// Только окно в фокусе: меньше шума для модели, меньше токенов и меньше
    /// лишнего покидает машину.
    Window,
    Screen,
}

impl Target {
    pub fn parse(value: &str) -> Self {
        match value {
            "screen" => Target::Screen,
            _ => Target::Window,
        }
    }
}

fn resize(image: RgbaImage, max_edge: u32) -> RgbaImage {
    let (w, h) = (image.width(), image.height());
    let scale = max_edge as f32 / w.max(h) as f32;
    if scale >= 1.0 {
        return image;
    }
    let (nw, nh) = (((w as f32 * scale) as u32).max(1), ((h as f32 * scale) as u32).max(1));
    image::imageops::resize(&image, nw, nh, image::imageops::FilterType::Triangle)
}

fn monitor_frame(max_edge: u32) -> Result<RgbaImage> {
    let monitors = xcap::Monitor::all().context("список мониторов")?;
    let monitor = monitors
        .into_iter()
        .find(|m| m.is_primary().unwrap_or(false))
        .or_else(|| xcap::Monitor::all().ok().and_then(|m| m.into_iter().next()))
        .context("монитор не найден")?;

    let image = monitor.capture_image().context("захват экрана")?;
    Ok(resize(image, max_edge))
}

/// Окно в фокусе. Наш оверлей сюда попасть не может: он с `WS_EX_NOACTIVATE`
/// и фокус не забирает никогда.
fn focused_window() -> Option<xcap::Window> {
    xcap::Window::all().ok()?.into_iter().find(|w| {
        w.is_focused().unwrap_or(false)
            && !w.is_minimized().unwrap_or(false)
            && w.width().unwrap_or(0) > 0
            && w.height().unwrap_or(0) > 0
    })
}

fn window_frame(max_edge: u32) -> Option<(RgbaImage, String)> {
    let window = focused_window()?;
    let title = window.title().unwrap_or_default();
    let app = window.app_name().unwrap_or_default();

    let image = match window.capture_image() {
        Ok(image) => image,
        Err(e) => {
            eprintln!("[ghost] окно «{title}» не снялось ({e}), беру экран");
            return None;
        }
    };

    let label = match (app.is_empty(), title.is_empty()) {
        (false, false) => format!("{app} — {title}"),
        (true, false) => title,
        (false, true) => app,
        (true, true) => "окно в фокусе".into(),
    };
    Some((resize(image, max_edge), label))
}

/// Снимок для модели со зрением.
///
/// Дальше 1568 точек по длинной стороне смысла нет: модели всё равно ужимают
/// вход, а мы платим за это токенами и временем передачи. PNG, а не JPEG:
/// артефакты сжатия портят мелкий текст, ради которого всё и затевается.
pub fn capture(max_edge: u32, target: Target) -> Result<Shot> {
    let (image, source) = match target {
        Target::Window => match window_frame(max_edge) {
            Some(pair) => pair,
            // Окна в фокусе может не быть вовсе — например, фокус на рабочем
            // столе. Отдать пустоту хуже, чем снять экран и честно сказать.
            None => (monitor_frame(max_edge)?, "весь экран (окно не найдено)".into()),
        },
        Target::Screen => (monitor_frame(max_edge)?, "весь экран".into()),
    };

    let (width, height) = (image.width(), image.height());
    let mut png = Vec::new();
    PngEncoder::new(&mut png)
        .write_image(image.as_raw(), width, height, ExtendedColorType::Rgba8)
        .context("кодирование PNG")?;

    Ok(Shot { png, width, height, source })
}

/// Кадр для раздачи помощнику. Здесь всегда весь экран: помощник должен видеть
/// обстановку целиком, а не то окно, в котором вы сейчас печатаете. И JPEG
/// вместо PNG — поток идёт несколько раз в секунду, вес кадра важнее точности.
pub fn capture_jpeg(max_edge: u32, quality: u8) -> Result<Vec<u8>> {
    let image = monitor_frame(max_edge)?;
    let (width, height) = (image.width(), image.height());

    // JPEG не знает про альфа-канал.
    let rgb: Vec<u8> = image.pixels().flat_map(|p| [p.0[0], p.0[1], p.0[2]]).collect();

    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, quality)
        .write_image(&rgb, width, height, ExtendedColorType::Rgb8)
        .context("кодирование JPEG")?;

    Ok(jpeg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn цель_снимка_разбирается_с_запасом() {
        assert!(matches!(Target::parse("screen"), Target::Screen));
        assert!(matches!(Target::parse("window"), Target::Window));
        // Незнакомое значение не должно приводить к съёмке всего экрана:
        // окно в фокусе — более осторожный выбор.
        assert!(matches!(Target::parse("мусор"), Target::Window));
    }
}
