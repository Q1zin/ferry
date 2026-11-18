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

pub fn run(device: &str, args: &[&str]) -> io::Result<i32> {
    let (session, args) = match args {
        ["--session", id, rest @ ..] => (crate::execution::session_name(id), rest),
        _ => (control::session(), args),
    };
    let Some(action) = action(args) else {
        return crate::usage();
    };
    crate::devices::finish((|| {
        let state = crate::devices::state()?;
        if !version::at_least(&state.me.version, version::ADMIN_WINDOW) {
            return Err(Fail::new(format!(
                "для окон администратора обнови пульт до ferry {}",
                version::ADMIN_WINDOW
            )));
        }
        match control::call_session(
            Request::AdminInput {
                device: device.into(),
                action,
            },
            &session,
        )? {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_window_actions() {
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
