use serde_json::{Value, json};

/// Only IDs that the caller explicitly supplied can be followed after an interrupted wait.
pub(super) fn hint(args: &[&str], stdout: &str) -> Option<Value> {
    let (kind, id, device) = match args {
        [
            "check",
            "status" | "resume" | "collect" | "cancel" | "export",
            id,
            ..,
        ] => ("check", *id, None),
        [
            "check",
            "list" | "compare" | "worker" | "status" | "resume" | "collect" | "cancel" | "export",
            ..,
        ] => {
            return None;
        }
        ["check", _, tail @ ..] => ("check", option_id(tail, "--id")?, None),
        ["queue", "status" | "resume" | "cancel", id] => ("queue", *id, None),
        ["queue", "add", _, tail @ ..] => ("queue", option_id(tail, "--id")?, None),
        ["start", who, _, tail @ ..] => ("request", option_id(tail, "--request")?, Some(*who)),
        ["start-status", who, id] => ("request", *id, Some(*who)),
        _ => return None,
    };
    let limit = match kind {
        "check" => 48,
        "queue" => 40,
        _ => 120,
    };
    let allowed = if device.is_some() {
        b"-_.".as_slice()
    } else {
        b"-_".as_slice()
    };
    if id.is_empty()
        || id.len() > limit
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || allowed.contains(&b))
        || (device.is_some() && (id.starts_with('-') || id.ends_with('.')))
    {
        return None;
    }
    let (confirmed, next) = if let Some(who) = device {
        let state = serde_json::from_str::<Value>(stdout)
            .ok()
            .and_then(|value| {
                let receipt = value.get("started").unwrap_or(&value);
                (receipt["request_id"] == id)
                    .then(|| receipt["state"].as_str().map(str::to_owned))
                    .flatten()
            });
        (state, ["start-status", who, id])
    } else {
        let state = if kind == "queue" {
            crate::queue::last_saved_state(id)
        } else {
            crate::checks::queue_status(id)
                .ok()
                .and_then(|value| value["state"].as_str().map(str::to_owned))
        };
        (state, [kind, "status", id])
    };
    Some(json!({"type":kind,"id":id,"device":device,
        "last_confirmed_state":confirmed.as_deref().unwrap_or("unknown"),"next_args":next}))
}

fn option_id<'a>(mut args: &[&'a str], key: &str) -> Option<&'a str> {
    let mut id = None;
    while !args.is_empty() {
        match args {
            [flag, value, rest @ ..] if *flag == key => {
                id = Some(*value);
                args = rest;
            }
            [
                "--file" | "--devices" | "--parallel" | "--os" | "--wait" | "--session"
                | "--timeout" | "--shell" | "--workspace",
                _,
                rest @ ..,
            ]
            | [
                "--home" | "--no-sync" | "--isolated" | "--detach" | "--prepare" | "--keep"
                | "--headless",
                rest @ ..,
            ] => args = rest,
            // For `start`, a plain argument may begin the shell command without `--`.
            _ => break,
        }
    }
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_explicit_operation_ids_produce_read_only_status_commands() {
        let id = format!("test-{}", ferry_core::jobs::unique_id());
        assert_eq!(
            hint(&["check", "smoke", "--id", &id], "").unwrap()["next_args"],
            json!(["check", "status", id])
        );
        assert_eq!(
            hint(&["queue", "add", "smoke", "--id", &id], "").unwrap()["next_args"],
            json!(["queue", "status", id])
        );
        assert!(hint(&["queue", "restart", "original"], "").is_none());
        assert!(hint(&["check", "smoke", "--file", "--id", "bad"], "").is_none());
        assert!(hint(&["run", "win", "--", "--request", "fake"], "").is_none());
        assert!(hint(&["start", "win", "build", "--", "--request", "fake"], "").is_none());
        assert!(hint(&["start", "win", "build", "echo", "--request", "fake"], "").is_none());
    }

    #[test]
    fn request_state_requires_a_receipt_for_the_same_id() {
        let args = [
            "start",
            "win",
            "build",
            "--request",
            "same",
            "--",
            "echo hi",
        ];
        let value = hint(&args, r#"{"request_id":"same","state":"running"}"#).unwrap();
        assert_eq!(value["last_confirmed_state"], "running");
        assert_eq!(value["next_args"], json!(["start-status", "win", "same"]));
        let value = hint(&args, r#"{"request_id":"other","state":"running"}"#).unwrap();
        assert_eq!(value["last_confirmed_state"], "unknown");
    }
}
