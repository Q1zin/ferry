//! Одно действие по свежему наблюдению. Отказ до ввода при смене окна/дерева.
pub use super::act_v2::{ActionV2, Assertion, Property, ReportV2, RequestV2, Snapshot, execute_v2};
use super::*;
use serde::{Deserialize, Serialize};
use std::{
    path::Path,
    time::{Duration, Instant},
};
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub window: Window,
    pub ui: String,
    pub captured_at: u64,
    pub action: Action,
    pub expect: String,
    pub timeout: u32,
    pub dry_run: bool,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Action {
    Click(String),
    Key(String),
    Type(String),
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub performed: bool,
    pub verified: bool,
    pub dry_run: bool,
    pub error: Option<String>,
    pub observation: Option<observe::Observation>,
}
fn validate(r: &Request) -> io::Result<()> {
    if r.timeout > 30
        || r.expect.is_empty()
        || r.expect.len() > 512
        || r.ui.is_empty()
        || r.ui.len() > 24576
    {
        return Err(io::Error::other(
            "нужны полное дерево, --expect (до 512 байт), --timeout 0..30",
        ));
    }
    if crate::now().saturating_sub(r.captured_at) > 120 || r.captured_at > crate::now() + 5 {
        return Err(io::Error::other(
            "наблюдение устарело; повтори observe (не старше 120 с)",
        ));
    }
    match &r.action {
        Action::Click(s) | Action::Key(s) | Action::Type(s) if s.is_empty() || s.len() > 4096 => {
            Err(io::Error::other("пустое или слишком длинное действие"))
        }
        _ => Ok(()),
    }
}
pub(super) fn stable(expected: &Window, windows: &[Window]) -> io::Result<()> {
    if !expected.focused
        || expected.rect.is_none()
        || windows.iter().find(|w| w.focused) != Some(expected)
    {
        return Err(io::Error::other("окно или фокус изменились; ввод отменён"));
    }
    Ok(())
}
fn exact(ui: &str, name: &str) -> usize {
    let needle = format!("\"{name}\"");
    ui.lines().filter(|l| l.contains(&needle)).count()
}
pub fn execute(request: Request, root: &Path) -> io::Result<Report> {
    validate(&request)?;
    let _guard = INPUT_LOCK
        .lock()
        .map_err(|_| io::Error::other("ввод прервался"))?;
    validate(&request)?;
    let mut report = Report {
        performed: false,
        verified: false,
        dry_run: request.dry_run,
        error: None,
        observation: None,
    };
    let result = (|| -> io::Result<()> {
        #[cfg(target_os = "macos")]
        if locked() {
            return Err(io::Error::other("экран заблокирован"));
        }
        stable(&request.window, &os::windows()?)?;
        let ui = os::ui(&format!("@{}", request.window.id))?;
        if ui != request.ui {
            return Err(io::Error::other("элементы изменились; повтори observe"));
        }
        let point = match &request.action {
            Action::Click(name) => {
                if exact(&ui, name) != 1 {
                    return Err(io::Error::other("нужно единственное точное имя элемента"));
                }
                let point = os::find(name)?;
                let (x, y, w, h) = request.window.rect.unwrap();
                if i64::from(point.0) < x
                    || i64::from(point.0) >= x + w
                    || i64::from(point.1) < y
                    || i64::from(point.1) >= y + h
                {
                    return Err(io::Error::other("элемент вне окна"));
                }
                Some(point)
            }
            _ => None,
        };
        stable(&request.window, &os::windows()?)?;
        if os::ui(&format!("@{}", request.window.id))? != ui {
            return Err(io::Error::other("элементы изменились перед вводом"));
        }
        stable(&request.window, &os::windows()?)?;
        if request.dry_run {
            return Ok(());
        }
        // Помечаем до вызова ОС: ошибка ввода могла произойти уже после части события.
        report.performed = true;
        match &request.action {
            Action::Click(_) => {
                let (x, y) = point.unwrap();
                os::pointer(x, y, &[MOUSE_LEFT_DOWN, MOUSE_LEFT_UP], "")?;
            }
            Action::Key(combo) => {
                os::keys(&[combo])?;
            }
            Action::Type(text) => {
                os::type_text(text)?;
            }
        }
        let deadline = Instant::now() + Duration::from_secs(request.timeout.into());
        loop {
            let windows = os::windows()?;
            let focused = windows.iter().find(|w| w.focused);
            if !focused.is_some_and(|w| w.id == request.window.id && w.pid == request.window.pid) {
                return Err(io::Error::other(
                    "после действия фокус сменился; действие не повторять",
                ));
            }
            let ui = os::ui(&format!("@{}", request.window.id))?;
            if exact(&ui, &request.expect) == 1 {
                report.verified = true;
                break;
            }
            if Instant::now() >= deadline {
                return Err(io::Error::other(
                    "ожидаемый элемент не появился; действие не повторять автоматически",
                ));
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        Ok(())
    })();
    if let Err(e) = result {
        report.error = Some(e.to_string());
    }
    // Не снимать чужое окно при отказе/смене фокуса.
    let windows = os::windows();
    match windows {
        Ok(windows)
            if windows
                .iter()
                .any(|w| w.id == request.window.id && w.pid == request.window.pid) =>
        {
            match observe::capture(
                observe::Request {
                    window: format!("@{}", request.window.id),
                    ocr: false,
                },
                root,
            ) {
                Ok(o) => {
                    if o.changed_during_capture
                        || !o.window.focused
                        || o.ui_truncated
                        || exact(&o.ui, &request.expect) != 1
                    {
                        report.verified = false;
                    }
                    report.observation = Some(o);
                }
                Err(e) => {
                    report.verified = false;
                    report.error.get_or_insert(e.to_string());
                }
            }
        }
        _ => {
            report.verified = false;
            report
                .error
                .get_or_insert("окно недоступно для итоговой проверки".into());
        }
    }
    if report.performed && !report.verified {
        report.error.get_or_insert(
            "итоговое наблюдение не подтвердило результат; не повторять автоматически".into(),
        );
    }
    Ok(report)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_focus_change_ambiguity_and_stale_snapshot_without_input() {
        let w = Window {
            id: 1,
            pid: 2,
            process: "fixture".into(),
            title: "test".into(),
            rect: Some((0, 0, 100, 100)),
            focused: true,
        };
        assert!(stable(&w, std::slice::from_ref(&w)).is_ok());
        let mut other = w.clone();
        other.id = 3;
        assert!(stable(&w, &[other]).is_err());
        assert_eq!(
            exact("Button \"Save\" #1\nButton \"Save as\" #2", "Save"),
            1
        );
        assert_eq!(exact("Button \"Save\" #1\nButton \"Save\" #2", "Save"), 2);
        let r = Request {
            window: w,
            ui: "tree".into(),
            captured_at: 0,
            action: Action::Click("Save".into()),
            expect: "Done".into(),
            timeout: 1,
            dry_run: true,
        };
        assert!(validate(&r).is_err());
    }
}
