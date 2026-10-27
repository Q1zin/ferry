//! Bounded native capture; recording is restricted to one window of an exact live run.
use serde::{Deserialize, Serialize};
use std::{io, path::Path};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub fps: u32,
    pub max_width: u32,
    pub quality: u8,
    pub target: Option<OwnedWindow>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OwnedWindow {
    pub job: String,
    pub run: String,
    pub title: String,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Frame {
    pub sequence: u64,
    pub captured_ms: u64,
    pub width: u32,
    pub height: u32,
    pub source_width: u32,
    pub source_height: u32,
    pub origin_x: i32,
    pub origin_y: i32,
    /// Base64 also keeps local JSON transport compact. One frame is at most 4 MiB.
    pub jpeg: String,
}
impl Request {
    pub fn validate(&self) -> io::Result<()> {
        if !(1..=20).contains(&self.fps)
            || !(320..=2560).contains(&self.max_width)
            || !(30..=90).contains(&self.quality)
        {
            return Err(io::Error::other(
                "stream: fps 1..20, max_width 320..2560, quality 30..90",
            ));
        }
        if let Some(t) = &self.target {
            crate::jobs::validate_name(&t.job)?;
            crate::jobs::validate_name(&t.run)?;
            if t.title.trim().is_empty() || t.title.len() > 1024 {
                return Err(io::Error::other("stream: нужен точный заголовок окна"));
            }
        }
        Ok(())
    }
}

#[cfg(any(windows, target_os = "macos"))]
pub fn frame(root: &Path, request: &Request, sequence: u64) -> io::Result<Frame> {
    use base64::Engine;
    use image::GenericImageView;
    request.validate()?;
    let (rgba, origin_x, origin_y) = if let Some(t) = &request.target {
        let probe = crate::jobs::evidence::execute(
            root,
            crate::jobs::evidence::Query {
                name: t.job.clone(),
                run_id: Some(t.run.clone()),
                action: crate::jobs::evidence::Action::Probe {
                    http: None,
                    window: Some(t.title.clone()),
                },
            },
        )?;
        if probe["running"] != true {
            return Err(io::Error::other("capture: запуск завершился"));
        }
        let windows: Vec<super::Window> = serde_json::from_value(probe["windows"].clone())?;
        let matches: Vec<_> = windows.iter().filter(|w| w.title == t.title).collect();
        if matches.len() != 1 {
            return Err(io::Error::other(
                "capture: требуется ровно одно окно с точным заголовком в указанном запуске",
            ));
        }
        let own = matches[0];
        let windows = xcap::Window::all().map_err(io::Error::other)?;
        let window = windows
            .into_iter()
            .find(|w| {
                w.id().ok().map(i64::from) == Some(own.id)
                    && w.pid().ok().map(i64::from) == Some(own.pid)
                    && w.title().ok().as_deref() == Some(t.title.as_str())
            })
            .ok_or_else(|| io::Error::other("capture: окно исчезло"))?;
        (window.capture_image().map_err(io::Error::other)?, 0, 0)
    } else {
        // Primary monitor only: this matches macOS's existing input coordinate space.
        let monitors = xcap::Monitor::all().map_err(io::Error::other)?;
        let monitor = monitors
            .iter()
            .find(|m| m.is_primary().unwrap_or(false))
            .ok_or_else(|| io::Error::other("stream: основной экран недоступен"))?;
        #[cfg(windows)]
        let origin = {
            let (x, y, _, _) = super::win::desktop();
            (
                monitor.x().map_err(io::Error::other)? - x,
                monitor.y().map_err(io::Error::other)? - y,
            )
        };
        #[cfg(not(windows))]
        let origin = (0, 0);
        (
            monitor.capture_image().map_err(io::Error::other)?,
            origin.0,
            origin.1,
        )
    };
    let captured_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let (source_width, source_height) = rgba.dimensions();
    let image = image::DynamicImage::ImageRgba8(rgba);
    let image = if source_width > request.max_width {
        image.resize(
            request.max_width,
            2560,
            image::imageops::FilterType::Triangle,
        )
    } else {
        image
    };
    let (width, height) = image.dimensions();
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, request.quality)
        .encode_image(&image.to_rgb8())
        .map_err(io::Error::other)?;
    if jpeg.len() > 4 * 1024 * 1024 {
        return Err(io::Error::other("stream: кадр больше 4 МиБ"));
    }
    Ok(Frame {
        sequence,
        captured_ms,
        width,
        height,
        source_width,
        source_height,
        origin_x,
        origin_y,
        jpeg: base64::engine::general_purpose::STANDARD.encode(jpeg),
    })
}
#[cfg(not(any(windows, target_os = "macos")))]
pub fn frame(_root: &Path, _request: &Request, _sequence: u64) -> io::Result<Frame> {
    Err(io::Error::other("stream поддерживает Windows и macOS"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_unbounded_capture_and_ambiguous_ownership() {
        let mut r = Request {
            fps: 10,
            max_width: 1280,
            quality: 70,
            target: None,
        };
        assert!(r.validate().is_ok());
        r.fps = 100;
        assert!(r.validate().is_err());
        r.fps = 10;
        r.target = Some(OwnedWindow {
            job: "../jobs".into(),
            run: "r".into(),
            title: "fixture".into(),
        });
        assert!(r.validate().is_err());
    }
}
