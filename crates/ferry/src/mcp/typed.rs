use serde_json::{Value, json};
use std::io;
pub(super) fn tools() -> Vec<Value> {
    vec![
        definition(
            "ferry_devices",
            "Find available devices and capabilities before starting work.",
            json!({"os":{"type":"string","enum":["windows","macos"]},"desktop":{"type":"boolean"},"tool":{"type":"string"}}),
            &[],
        ),
        definition(
            "ferry_check_start",
            "Start a durable test operation. Supply a unique id; retry the same id after a lost response. Poll ferry_check_status.",
            json!({"profile":{"type":"string"},"id":{"type":"string"},"devices":{"type":"string"},"file":{"type":"string"},"isolated":{"type":"boolean"}}),
            &["profile", "id", "devices"],
        ),
        definition(
            "ferry_check_status",
            "Read bounded structured status of a durable operation; includes phases, tests and artifact references.",
            json!({"id":{"type":"string"}}),
            &["id"],
        ),
        definition(
            "ferry_check_history",
            "List recent operations with an optional cursor.",
            json!({"limit":{"type":"integer","minimum":1,"maximum":100},"before":{"type":"string"}}),
            &[],
        ),
        definition(
            "ferry_check_control",
            "Cancel or resume the same operation; export gathers a portable report and evidence bundle.",
            json!({"id":{"type":"string"},"action":{"type":"string","enum":["cancel","resume","export"]}}),
            &["id", "action"],
        ),
        definition(
            "ferry_check_compare",
            "Compare test identities and outcomes between two operations, including regressions and incomplete evidence.",
            json!({"base":{"type":"string"},"candidate":{"type":"string"}}),
            &["base", "candidate"],
        ),
        definition(
            "ferry_observe",
            "Read a device window as structured UI state and image. Never acts on the desktop.",
            json!({"device":{"type":"string"},"window":{"type":"string"},"screenshot":{"type":"boolean"}}),
            &["device"],
        ),
    ]
}
fn definition(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({"name":name,"description":description,"inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false}})
}
pub(super) fn args(name: &str, a: &Value) -> io::Result<Vec<String>> {
    let schema = tools()
        .into_iter()
        .find(|v| v["name"] == name)
        .ok_or_else(|| io::Error::other("unknown tool"))?;
    let values = a
        .as_object()
        .ok_or_else(|| io::Error::other("arguments must be an object"))?;
    let props = schema["inputSchema"]["properties"].as_object().unwrap();
    for (k, v) in values {
        let p = props
            .get(k)
            .ok_or_else(|| io::Error::other(format!("unknown argument: {k}")))?;
        let valid = match p["type"].as_str().unwrap() {
            "string" => v
                .as_str()
                .is_some_and(|s| !s.is_empty() && s.len() <= 4096 && !s.contains('\0')),
            "boolean" => v.is_boolean(),
            "integer" => v.as_u64().is_some_and(|n| (1..=100).contains(&n)),
            _ => false,
        };
        if !valid || p["enum"].as_array().is_some_and(|vs| !vs.contains(v)) {
            return Err(io::Error::other(format!("invalid argument: {k}")));
        }
    }
    for k in schema["inputSchema"]["required"].as_array().unwrap() {
        if !values.contains_key(k.as_str().unwrap()) {
            return Err(io::Error::other(format!("missing argument: {k}")));
        }
    }
    // Positional values must never be interpreted as another CLI action or option.
    for key in ["device", "devices", "profile", "id", "base", "candidate"] {
        if let Some(value) = a[key].as_str()
            && (value.starts_with('-') || value.chars().any(char::is_control))
        {
            return Err(io::Error::other(format!(
                "invalid positional argument: {key}"
            )));
        }
    }
    if name == "ferry_check_start"
        && matches!(
            a["profile"].as_str(),
            Some(
                "list"
                    | "status"
                    | "resume"
                    | "collect"
                    | "cancel"
                    | "worker"
                    | "compare"
                    | "export"
            )
        )
    {
        return Err(io::Error::other("profile name is reserved by check CLI"));
    }
    let s = |key: &str| a[key].as_str().unwrap_or_default().to_string();
    let mut result = match name {
        "ferry_devices" => vec!["select".into()],
        "ferry_check_start" => vec![
            "check".into(),
            s("profile"),
            "--id".into(),
            s("id"),
            "--devices".into(),
            s("devices"),
            "--detach".into(),
        ],
        "ferry_check_status" => vec!["check".into(), "status".into(), s("id")],
        "ferry_check_history" => vec!["check".into(), "list".into()],
        "ferry_check_control" => vec!["check".into(), s("action"), s("id")],
        "ferry_check_compare" => vec!["check".into(), "compare".into(), s("base"), s("candidate")],
        "ferry_observe" => vec!["observe".into(), s("device"), "--structured".into()],
        _ => unreachable!(),
    };
    let keys: &[&str] = match name {
        "ferry_devices" => &["os", "tool"],
        "ferry_check_start" => &["file"],
        "ferry_check_history" => &["limit", "before"],
        "ferry_observe" => &["window"],
        _ => &[],
    };
    for k in keys {
        if let Some(v) = a.get(k) {
            result.push(format!("--{k}"));
            result.push(
                v.as_str()
                    .map(String::from)
                    .unwrap_or_else(|| v.to_string()),
            );
        }
    }
    if a["desktop"] == true {
        result.push("--desktop".into());
    }
    if a["isolated"] == true {
        result.push("--isolated".into());
    }
    if a["screenshot"] == false {
        result.push("--no-screenshot".into());
    }
    if name == "ferry_check_control" && matches!(a["action"].as_str(), Some("resume" | "cancel")) {
        result.push("--detach".into());
    }
    Ok(result)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn typed_targets_cannot_turn_into_local_flags_or_check_subcommands() {
        assert!(args("ferry_observe", &json!({"device":"--structured"})).is_err());
        for profile in [
            "cancel",
            "status",
            "worker",
            "list",
            "export",
            "compare",
            "--prepare",
        ] {
            assert!(
                args(
                    "ferry_check_start",
                    &json!({"profile":profile,"id":"one","devices":"win"})
                )
                .is_err()
            );
        }
        assert!(
            args(
                "ferry_check_start",
                &json!({"profile":"smoke","id":"one","devices":"WIN-11-2"})
            )
            .is_ok()
        );
        assert_eq!(
            args(
                "ferry_check_control",
                &json!({"id":"one","action":"cancel"})
            )
            .unwrap()
            .last()
            .unwrap(),
            "--detach"
        );
    }
    #[test]
    fn validates_before_constructing_argv() {
        assert!(
            args(
                "ferry_check_start",
                &json!({"id":"x","devices":"win","profile":7})
            )
            .is_err()
        );
        assert!(args("ferry_check_control", &json!({"id":"x","action":"delete"})).is_err());
        assert!(args("ferry_check_status", &json!({"id":"x","shell":"rm"})).is_err());
        assert_eq!(
            args("ferry_check_status", &json!({"id":"x"})).unwrap(),
            vec!["check", "status", "x"]
        );
    }
}
