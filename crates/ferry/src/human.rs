use crate::{agent_tools::report, devices};
use ferry_core::{control::Request, human::Request as Human};
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

pub fn desk_take(device: &str, args: &[&str]) -> io::Result<i32> {
    let (mut minutes, mut note, mut rest) = (30u32, None, args);
    if let [value, tail @ ..] = rest
        && !value.starts_with('-')
    {
        minutes = value.parse().map_err(io::Error::other)?;
        rest = tail;
    }
    if let ["--note", text] = rest {
        note = Some((*text).to_string());
    } else if !rest.is_empty() {
        return crate::usage();
    }
    match note {
        None => devices::desk(device, "take", minutes),
        Some(note) => devices::finish((|| match devices::ask(Request::DeskNote {
            device: device.into(),
            minutes,
            note,
        })? {
            ferry_core::control::Response::Message(s) => {
                println!("{s}");
                Ok(0)
            }
            _ => Err(ferry_core::device::Fail::new("нет ответа о брони")),
        })()),
    }
}
