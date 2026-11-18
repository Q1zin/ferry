use std::io;

use ferry_core::control::{self, Request, Response};
use ferry_core::device::{Fail, version};
use ferry_core::screen::admin::Action;

fn action(args: &[&str]) -> Option<Action> {
    Some(match args {
        ["status"] => Action::Probe,
        ["focus"] => Action::Focus,
        ["move", x, y] => Action::Move {
            x: x.parse().ok()?,
            y: y.parse().ok()?,
        },
        ["click", x, y, rest @ ..] if rest.is_empty() || *rest == ["right"] => Action::Click {
            x: x.parse().ok()?,
            y: y.parse().ok()?,
            right: !rest.is_empty(),
        },
        ["scroll", count] => Action::Scroll {
            clicks: count.parse().ok()?,
        },
        ["key", combo] => Action::Key {
            combo: (*combo).into(),
        },
        ["type", text @ ..] if !text.is_empty() => Action::Type {
            text: text.join(" "),
        },
        _ => return None,
    })
}

pub fn run(device: &str, mut args: &[&str]) -> io::Result<i32> {
    let mut session = None;
    let mut window = None;
    loop {
        args = match args {
            ["--session", id, rest @ ..] if session.is_none() => {
                session = Some(crate::execution::session_name(id));
                rest
            }
            ["--window", id, rest @ ..] if window.is_none() => {
                let Some(id) = window_id(id) else {
                    return crate::usage();
                };
                window = Some(id);
                rest
            }
            _ => break,
        };
    }
    let Some(action) = action(args) else {
        return crate::usage();
    };
    crate::devices::finish((|| {
        let state = crate::devices::state()?;
        let minimum = if window.is_some() {
            version::ADMIN_WINDOWS
        } else {
            version::ADMIN_WINDOW
        };
        if !version::at_least(&state.me.version, minimum) {
            return Err(Fail::new(format!(
                "для окон администратора обнови пульт до ferry {minimum}"
            )));
        }
        let request = match window {
            Some(window) => Request::AdminInputForWindow {
                device: device.into(),
                window,
                action,
            },
            None => Request::AdminInput {
                device: device.into(),
                action,
            },
        };
        match control::call_session(request, &session.unwrap_or_else(control::session))? {
            Response::Message(text) => {
                if !text.is_empty() {
                    println!("{text}");
                }
                Ok(0)
            }
            Response::Error(error) => Err(error),
            _ => Err(Fail::new("нет ответа окна администратора")),
        }
    })())
}

fn window_id(value: &str) -> Option<i64> {
    let id: i64 = value.strip_prefix('@').unwrap_or(value).parse().ok()?;
    (id > 0).then_some(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_window_actions() {
        assert_eq!(window_id("@1704708"), Some(1704708));
        assert_eq!(window_id("0"), None);
        assert_eq!(window_id("@-1"), None);
        assert!(matches!(
            action(&["click", "100", "25", "right"]),
            Some(Action::Click {
                x: 100,
                y: 25,
                right: true
            })
        ));
        assert!(action(&["click", "--name", "OK"]).is_none());
        assert!(action(&["key", "F12", "--for", "4"]).is_none());
        assert!(action(&["run", "powershell"]).is_none());
    }
}
