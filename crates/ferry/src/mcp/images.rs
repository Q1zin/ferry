//! Bounded image attachments and the translation back to desktop coordinates.
use image::{ImageFormat, ImageReader, Limits, codecs::jpeg::JpegEncoder};
use serde_json::{Value, json};
use std::{
    fs::File,
    io::{self, BufReader, Read},
    path::Path,
};

const LIMIT: u64 = 4 * 1024 * 1024;
const MAX_SOURCE_BYTES: u64 = 128 * 1024 * 1024;

pub(super) enum Mapping {
    Screen,
    Rect([i64; 4]),
    Unknown,
}

pub(super) fn append(parts: &mut Vec<Value>, path: &str, mapping: Mapping) -> Value {
    match attachment(Path::new(path), mapping) {
        Ok((image, details)) => {
            parts.push(image);
            if details["preview"] == true {
                parts.push(super::text(&format!(
                    "Превью снимка. Оригинал и координаты: {details}"
                )));
            }
            details
        }
        Err(err) => {
            parts.push(super::text(&format!(
                "{path}: не удалось приложить снимок ({err})"
            )));
            json!({"original_path": path, "error": err.to_string()})
        }
    }
}

pub(super) fn rect(value: &Value) -> Option<[i64; 4]> {
    serde_json::from_value::<[i64; 4]>(value.clone())
        .ok()
        .filter(|&[_, _, w, h]| w > 0 && h > 0)
}

fn reader(path: &Path) -> io::Result<ImageReader<BufReader<File>>> {
    let file = File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > MAX_SOURCE_BYTES {
        return Err(io::Error::other("ожидался файл PNG не больше 128 МиБ"));
    }
    let mut reader = ImageReader::with_format(BufReader::new(file), ImageFormat::Png);
    let mut limits = Limits::default();
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    limits.max_alloc = Some(160 * 1024 * 1024);
    reader.limits(limits);
    Ok(reader)
}

fn attachment(path: &Path, mapping: Mapping) -> io::Result<(Value, Value)> {
    let (width, height) = reader(path)?.into_dimensions().map_err(io::Error::other)?;
    if u64::from(width) * u64::from(height) > 40_000_000 {
        return Err(io::Error::other("снимок больше 40 млн пикселей"));
    }
    let file = File::open(path)?;
    let (bytes, kind, shown, preview) = if file.metadata()?.len() <= LIMIT {
        let mut bytes = Vec::new();
        file.take(LIMIT + 1).read_to_end(&mut bytes)?;
        (bytes, "image/png", [width, height], false)
    } else {
        let image = reader(path)?.decode().map_err(io::Error::other)?;
        let bound = (width.max(height) - 1).clamp(1, 1280);
        let thumbnail = image.thumbnail(bound, bound);
        let mut bytes = Vec::new();
        JpegEncoder::new_with_quality(&mut bytes, 82)
            .encode_image(&thumbnail)
            .map_err(io::Error::other)?;
        (
            bytes,
            "image/jpeg",
            [thumbnail.width(), thumbnail.height()],
            true,
        )
    };
    if bytes.len() as u64 > LIMIT {
        return Err(io::Error::other("превью превышает лимит 4 МиБ"));
    }
    let rect = match mapping {
        Mapping::Screen => Some([0, 0, i64::from(width), i64::from(height)]),
        Mapping::Rect(rect) => Some(rect),
        Mapping::Unknown => None,
    };
    let mapping = rect.map(|[x, y, w, h]| {
        json!({
            "origin_x": x, "origin_y": y,
            "desktop_pixels_per_image_pixel_x": w as f64 / f64::from(shown[0]),
            "desktop_pixels_per_image_pixel_y": h as f64 / f64::from(shown[1]),
        })
    });
    let details = json!({
        "original_path": path,
        "original_size": [width, height],
        "image_size": shown,
        "preview": preview,
        "desktop_mapping": mapping,
    });
    let image = json!({
        "type": "image",
        "mimeType": kind,
        "data": base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes),
    });
    Ok((image, details))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn large_png_gets_smaller_preview_and_coordinates_for_an_offset_region() {
        let path = std::env::temp_dir().join(format!(
            "ferry-preview-{}.png",
            ferry_core::jobs::unique_id()
        ));
        let mut noise = 1_u32;
        let bytes = (0..1536 * 1024 * 3)
            .map(|_| {
                noise ^= noise << 13;
                noise ^= noise >> 17;
                noise ^= noise << 5;
                noise as u8
            })
            .collect();
        image::RgbImage::from_raw(1536, 1024, bytes)
            .unwrap()
            .save(&path)
            .unwrap();
        let metadata = path.metadata().unwrap();
        assert!(metadata.len() > LIMIT);
        let (image, details) = attachment(&path, Mapping::Rect([90, -42, 1536, 1024])).unwrap();
        assert_eq!(image["mimeType"], "image/jpeg");
        assert_eq!(details["preview"], true);
        assert_eq!(details["original_path"], json!(path));
        assert_eq!(details["original_size"], json!([1536, 1024]));
        assert_eq!(details["desktop_mapping"]["origin_x"], 90);
        assert_eq!(details["desktop_mapping"]["origin_y"], -42);
        let pixels = details["image_size"][0].as_f64().unwrap();
        let scale = details["desktop_mapping"]["desktop_pixels_per_image_pixel_x"]
            .as_f64()
            .unwrap();
        assert!(pixels < 1536.0);
        assert!((pixels * scale - 1536.0).abs() < 0.001);
        assert_eq!(path.metadata().unwrap().len(), metadata.len());
        std::fs::remove_file(path).unwrap();
    }
}
