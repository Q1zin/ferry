use super::*;
use serde::{Deserialize, Serialize};
use std::{fs, path::Path, time::Instant};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub window: String,
    pub ocr: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    pub captured_at: u64,
    pub elapsed_ms: u64,
    pub window: Window,
    pub foreground: Option<Window>,
    pub changed_during_capture: bool,
    pub screen_access: Option<bool>,
    pub input_access: Option<bool>,
    pub ui: String,
    pub ui_truncated: bool,
    pub text: Vec<crate::ocr::Text>,
    pub text_truncated: bool,
    pub warnings: Vec<String>,
    pub png: Vec<u8>,
}

pub fn capture(request: Request, root: &Path) -> io::Result<Observation> {
    let begin = Instant::now();
    #[cfg(target_os = "macos")]
    if locked() {
        return Err(io::Error::other("экран заблокирован"));
    }
    let windows = os::windows()?;
    let window = if request.window.is_empty() {
        windows
            .iter()
            .find(|w| w.focused && w.rect.is_some())
            .ok_or_else(|| io::Error::other("нет видимого окна с фокусом"))?
    } else {
        find_window(&windows, &request.window)?
    }
    .clone();
    let foreground = windows.iter().find(|w| w.focused).cloned();
    let (screen_access, input_access) = permissions();
    let mut result = Observation {
        captured_at: crate::now(),
        elapsed_ms: 0,
        window: window.clone(),
        foreground,
        changed_during_capture: false,
        screen_access,
        input_access,
        ui: String::new(),
        ui_truncated: false,
        text: Vec::new(),
        text_truncated: false,
        warnings: Vec::new(),
        png: Vec::new(),
    };
    let folder = root.join("observe-tmp");
    fs::create_dir_all(&folder)?;
    let file = folder.join(format!(
        "{}-{}.png",
        std::process::id(),
        crate::jobs::unique_id()
    ));
    let shot = (|| -> io::Result<Vec<u8>> {
        if screen_access == Some(false) {
            return Err(io::Error::other("нет разрешения записи экрана"));
        }
        let (_, _, width, height) = window.rect.unwrap_or_default();
        if width <= 0 || height <= 0 || width.saturating_mul(height) > 40_000_000 {
            return Err(io::Error::other("окно слишком большое для observe"));
        }
        if screenshot_selected(&file, &window)? != 0 {
            return Err(io::Error::other("снимок окна не получился"));
        }
        if fs::metadata(&file)?.len() > 8 * 1024 * 1024 {
            return Err(io::Error::other("снимок больше 8 МиБ"));
        }
        fs::read(&file)
    })();
    match shot {
        Ok(png) => result.png = png,
        Err(e) => result.warnings.push(e.to_string()),
    }
    if input_access == Some(false) {
        result.warnings.push("нет разрешения Accessibility".into());
    } else {
        match os::ui(&format!("@{}", window.id)) {
            Ok(ui) => {
                result.ui_truncated = ui.len() > 24576 || ui.contains("1500");
                result.ui = crate::junit::clip(&ui, 24576);
            }
            Err(e) => result.warnings.push(format!("элементы: {e}")),
        }
    }
    if request.ocr && !result.png.is_empty() {
        match crate::ocr::read(&file, "") {
            Ok(mut text) => {
                result.text_truncated = text.len() > 200;
                text.truncate(200);
                for line in &mut text {
                    if line.text.len() > 512 {
                        result.text_truncated = true;
                        line.text = crate::junit::clip(&line.text, 512);
                    }
                }
                result.text = text;
            }
            Err(e) => result.warnings.push(format!("OCR: {e}")),
        }
    }
    fs::remove_file(file).ok();
    match os::windows() {
        Ok(after) => result.changed_during_capture = changed(&windows, &after, &window),
        Err(e) => {
            result.changed_during_capture = true;
            result
                .warnings
                .push(format!("повторная проверка окна: {e}"));
        }
    }
    if result.changed_during_capture {
        result.warnings.push(
            "окно или фокус изменились во время сбора; перед действием обнови observe".into(),
        );
    }
    result.elapsed_ms = begin.elapsed().as_millis() as u64;
    Ok(result)
}
fn changed(before: &[Window], after: &[Window], selected: &Window) -> bool {
    after.iter().find(|w| w.id == selected.id) != Some(selected)
        || before.iter().find(|w| w.focused).map(|w| w.id)
            != after.iter().find(|w| w.focused).map(|w| w.id)
}
fn permissions() -> (Option<bool>, Option<bool>) {
    #[cfg(target_os = "macos")]
    {
        let (s, i) = access();
        (Some(s), Some(i))
    }
    #[cfg(not(target_os = "macos"))]
    {
        (None, None)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn detects_focus_motion_and_disappearance_without_real_input() {
        let w = Window {
            id: 1,
            pid: 2,
            process: "test".into(),
            title: "one".into(),
            rect: Some((0, 0, 20, 20)),
            focused: true,
        };
        assert!(!changed(
            std::slice::from_ref(&w),
            std::slice::from_ref(&w),
            &w
        ));
        let mut moved = w.clone();
        moved.rect = Some((10, 0, 20, 20));
        assert!(changed(std::slice::from_ref(&w), &[moved], &w));
        assert!(changed(std::slice::from_ref(&w), &[], &w));
    }
}
