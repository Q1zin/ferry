use crate::{agent_tools::report, devices};
use ferry_core::{
    control::{self, Request, Response},
    device::Fail,
    human::Request as Human,
};
use std::{
    io,
    time::{Duration, Instant},
};

pub fn ask(args: &[&str]) -> io::Result<i32> {
    let [device, tail @ ..] = args else {
        return crate::usage();
    };
    let (text, mut poll, mut cancel, mut rest) = match tail {
        ["--poll", id, rest @ ..] => (None, Some((*id).to_string()), None, rest),
        ["--cancel", id, rest @ ..] => (None, None, Some((*id).to_string()), rest),
        [text, rest @ ..] if !text.starts_with('-') => {
            (Some((*text).to_string()), None, None, rest)
        }
        _ => return crate::usage(),
    };
    let (mut id, mut timeout, mut wait) = (None, 600u32, 0u64);
    while !rest.is_empty() {
        match rest {
            ["--id", value, tail @ ..] if text.is_some() => {
                id = Some((*value).to_string());
                rest = tail;
            }
            ["--timeout", value, tail @ ..] if text.is_some() => {
                timeout = value.parse().map_err(io::Error::other)?;
                rest = tail;
            }
            ["--wait", value, tail @ ..] if cancel.is_none() => {
                wait = value.parse().map_err(io::Error::other)?;
                rest = tail;
            }
            _ => return crate::usage(),
        }
    }
    if wait > 60 {
        return Err(io::Error::other("--wait: 0..60 с"));
    }
    devices::finish((|| {
        let request = if let Some(id) = cancel.take() {
            Human::Cancel { id }
        } else if let Some(id) = poll.take() {
            Human::Poll { id }
        } else {
            Human::Ask {
                id: id.unwrap_or_else(ferry_core::jobs::unique_id),
                text: text.unwrap(),
                timeout,
            }
        };
        let target = (*device != "local").then(|| (*device).to_string());
        let send = |request| {
            report(Request::Human {
                device: target.clone(),
                request,
            })
        };
        let deadline = Instant::now() + Duration::from_secs(wait);
        let mut value = send(request)?;
        while value["state"] == "pending" && Instant::now() < deadline {
            std::thread::sleep(
                Duration::from_millis(500).min(deadline.saturating_duration_since(Instant::now())),
            );
            value = send(Human::Poll {
                id: value["id"].as_str().unwrap_or_default().into(),
            })?;
        }
        value["device"] = (*device).into();
        value["requires_verification"] = true.into();
        let failed = matches!(
            value["state"].as_str(),
            Some("declined" | "expired" | "cancelled")
        );
        println!("{value}");
        Ok(i32::from(failed))
    })())
}

pub fn desk(device: &str, args: &[&str]) -> io::Result<i32> {
    let (action, minutes, mut rest) = match args {
        ["take", minutes, tail @ ..] if !minutes.starts_with('-') => {
            ("take", minutes.parse().map_err(io::Error::other)?, tail)
        }
        ["take", tail @ ..] => ("take", 30, tail),
        ["free", tail @ ..] => ("free", 0, tail),
        _ => ("show", 0, args),
    };
    let (mut session, mut note, mut json, mut force, mut keep_awake) =
        (control::session(), None, false, false, false);
    while !rest.is_empty() {
        match rest {
            ["--session", id, tail @ ..] => {
                session = crate::execution::session_name(id);
                rest = tail;
            }
            ["--note", text, tail @ ..] if action == "take" => {
                note = Some((*text).to_string());
                rest = tail;
            }
            ["--keep-awake", tail @ ..] if action == "take" => {
                keep_awake = true;
                rest = tail;
            }
            ["--json", tail @ ..] if action == "show" => {
                json = true;
                rest = tail;
            }
            ["--force", tail @ ..] if action == "free" => {
                force = true;
                rest = tail;
            }
            _ => return crate::usage(),
        }
    }
    let request = if keep_awake {
        if !(1..=720).contains(&minutes) {
            return devices::finish(Err(Fail::new("--keep-awake: срок от 1 до 720 минут")));
        }
        let state = match devices::state() {
            Ok(state) => state,
            Err(error) => return devices::finish(Err(error)),
        };
        use ferry_core::device::version::{DESK_AWAKE, at_least};
        if !at_least(&state.me.version, DESK_AWAKE) {
            return devices::finish(Err(Fail::new(format!(
                "для брони без сна обнови приложение на пульте до ferry {DESK_AWAKE}"
            ))));
        }
        Request::DeskAwake {
            device: device.into(),
            minutes,
            note,
        }
    } else if let Some(note) = note {
        Request::DeskNote {
            device: device.into(),
            minutes,
            note,
        }
    } else {
        let action = if json {
            "show-json"
        } else if force {
            "force"
        } else {
            action
        };
        Request::Desk {
            device: device.into(),
            minutes,
            action: action.into(),
        }
    };
    devices::finish(match control::call_session(request, &session) {
        Ok(Response::Message(text)) => {
            println!("{device}: {text}");
            Ok(0)
        }
        Ok(Response::Report(text)) => {
            println!("{text}");
            Ok(0)
        }
        Ok(Response::Error(e)) => Err(e),
        Ok(_) => Err(Fail::new("нет ответа о брони")),
        Err(e) => Err(e.into()),
    })
}
