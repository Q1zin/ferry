//! Профиль → отдельный запуск → связанные доказательства → частичный отчёт матрицы.
use crate::{
    devices::{self, finish, state},
    diagnostics,
};
use ferry_core::{
    control::{self, Request, Response},
    desk::Shell,
    device::{Fail, Status},
    jobs::{self, evidence::Action},
    mode::Mode,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, io, time::Duration};

#[derive(Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Spec {
    command: String,
    #[serde(default)]
    build: Option<String>,
    #[serde(default)]
    launch: Option<String>,
    #[serde(default)]
    shell: Shell,
    #[serde(default)]
    tools: Vec<String>,
}
#[derive(Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Profile {
    platforms: BTreeMap<String, Spec>,
    #[serde(default)]
    headless: bool,
    #[serde(default)]
    ready_http: Option<HttpReady>,
    #[serde(default)]
    ready_window: Option<String>,
    #[serde(default = "default_timeout")]
    timeout: u32,
    #[serde(default)]
    ready_log: Option<String>,
    #[serde(default)]
    ready_port: Option<u16>,
    #[serde(default)]
    junit: Option<String>,
    #[serde(default)]
    artifacts: Vec<String>,
    #[serde(default)]
    window: Option<String>,
}
#[derive(Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct HttpReady {
    port: u16,
    #[serde(default = "http_path")]
    path: String,
    #[serde(default = "http_status")]
    status: u16,
    /// Уникальный токен {run} связывает ответ с нашим запуском.
    contains: String,
}
fn http_path() -> String {
    "/".into()
}
fn http_status() -> u16 {
    200
}
fn default_timeout() -> u32 {
    300
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    profiles: BTreeMap<String, Profile>,
}
fn validate(name: &str, p: &Profile) -> Result<(), Fail> {
    if name.is_empty()
        || name.len() > 40
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_".contains(c))
    {
        return Err(Fail::new("имя профиля: до 40 букв, цифр, - и _"));
    }
    if !(1..=3600).contains(&p.timeout) || p.artifacts.len() > 32 || p.platforms.is_empty() {
        return Err(Fail::new(
            "timeout 1..3600, до 32 артефактов, нужны platforms",
        ));
    }
    if p.ready_log
        .as_ref()
        .is_some_and(|s| s.is_empty() || s.len() > 256)
        || p.ready_port == Some(0)
        || p.window.as_ref().is_some_and(|w| w.trim().is_empty())
        || p.ready_window
            .as_ref()
            .is_some_and(|w| w.trim().is_empty() || w.len() > 256)
        || (p.headless && (p.window.is_some() || p.ready_window.is_some()))
    {
        return Err(Fail::new(
            "пустое условие готовности/окно или неверный порт",
        ));
    }
    if let Some(h) = &p.ready_http
        && (h.port == 0
            || !h.path.starts_with('/')
            || h.path.len() > 512
            || h.path.bytes().any(|b| b <= 32 || b >= 127)
            || !(100..=599).contains(&h.status)
            || !h.contains.contains("{run}")
            || h.contains.len() > 256)
    {
        return Err(Fail::new(
            "ready_http: port, path, status и contains с уникальным {run}",
        ));
    }
    for path in p.artifacts.iter().chain(p.junit.iter()) {
        if path.is_empty()
            || path.len() > 240
            || path
                .split('/')
                .any(|c| c.is_empty() || c == "." || c == "..")
            || path.contains(['\\', ':', '\0'])
        {
            return Err(Fail::new(
                "артефакты: относительные пути внутри {output}, без .. и обратных слешей",
            ));
        }
    }
    for (os, s) in &p.platforms {
        if !["macos", "windows", "all"].contains(&os.as_str())
            || s.command.is_empty()
            || s.command.len() > 32768
            || s.build
                .iter()
                .chain(s.launch.iter())
                .any(|c| c.trim().is_empty() || c.len() > 32768)
        {
            return Err(Fail::new(
                "platforms: macos, windows или all; нужна команда до 32 КиБ",
            ));
        }
        if (p.ready_http.is_some() || p.ready_window.is_some()) && s.launch.is_none() {
            return Err(Fail::new("ready_http/ready_window требуют launch"));
        }
        if p.ready_http.is_some() && s.launch.as_ref().is_none_or(|s| !s.contains("{run}")) {
            return Err(Fail::new(
                "launch должен передать уникальный {run} в HTTP-ответ готовности",
            ));
        }
        if s.launch.is_some()
            && p.ready_log.is_none()
            && p.ready_port.is_none()
            && p.ready_http.is_none()
            && p.ready_window.is_none()
        {
            return Err(Fail::new("launch требует явное условие готовности"));
        }
        if (!p.artifacts.is_empty() || p.junit.is_some()) && !s.command.contains("{output}") {
            return Err(Fail::new(
                "команда должна писать отчёты в {output}, уникальный каталог этого запуска",
            ));
        }
        ferry_core::capabilities::validate(&ferry_core::capabilities::Request {
            shell: s.shell,
            tools: s.tools.clone(),
            desktop: false,
        })?;
    }
    Ok(())
}
pub(crate) fn send(request: Request, session: &str) -> Result<Response, Fail> {
    match control::call_session(request, session)? {
        Response::Error(e) => Err(e),
        r => Ok(r),
    }
}
fn command(spec: &Spec, output: &str) -> String {
    let cmd = spec.command.replace("{output}", output);
    match spec.shell {
        Shell::Powershell => format!(
            "try {{ New-Item -ItemType Directory -Path '{output}' -Force -ErrorAction Stop | Out-Null }} catch {{ Write-Error $_; exit 1 }}; {cmd}"
        ),
        Shell::Cmd => format!(
            "(if not exist {0} mkdir {0}) && ({cmd})",
            output.replace('/', "\\")
        ),
        _ => format!("mkdir -p '{output}' && (\n{cmd}\n)"),
    }
}
fn preflight(
    device: &Option<String>,
    status: &Status,
    spec: &Spec,
    p: &Profile,
    session: &str,
) -> Result<Value, Fail> {
    let version = status
        .version
        .split('.')
        .map(str::parse::<u32>)
        .collect::<Result<Vec<_>, _>>();
    if !version.is_ok_and(|v| v.len() == 3 && v.as_slice() >= [0, 1, 23].as_slice()) {
        return Err(Fail::new("check требует ferry 0.1.23 на устройстве"));
    }
    if (!p.headless && status.mode != Mode::Active)
        || status.mode == Mode::Paused
        || !status.role.accepts_commands()
    {
        return Err(Fail::refused("устройство не принимает фоновые проверки"));
    }
    let request = ferry_core::capabilities::Request {
        shell: spec.shell,
        tools: spec.tools.clone(),
        desktop: p.window.is_some() || p.ready_window.is_some(),
    };
    let cap = match device {
        Some(d) => match send(
            Request::Capabilities {
                device: d.clone(),
                request,
            },
            session,
        )? {
            Response::Capabilities(c) => c,
            _ => return Err(Fail::new("нет возможностей устройства")),
        },
        None => ferry_core::capabilities::collect(request)?,
    };
    if !cap.shell_available
        || !cap.missing_tools.is_empty()
        || !cap.warnings.is_empty()
        || (p.window.is_some() && cap.locked != Some(false))
    {
        return Err(Fail::new(format!(
            "устройство не готово: {}",
            serde_json::to_string(&cap).unwrap_or_default()
        )));
    }
    Ok(json!(cap))
}
mod cli;
mod engine;
mod journal;
pub use cli::run;

fn tests_pass(t: &Value) -> bool {
    t["tests"].as_u64().unwrap_or(0) > 0
        && t["failed"] == 0
        && t["errors"] == 0
        && t["warnings"].as_array().is_some_and(|a| a.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn profiles_reject_stale_outputs_and_unknown_fields() {
        let mut p: Profile = serde_json::from_value(
            json!({"platforms":{"all":{"command":"test"}},"junit":"junit.xml"}),
        )
        .unwrap();
        assert!(validate("smoke", &p).is_err());
        p.platforms.get_mut("all").unwrap().command = "test --output {output}".into();
        assert!(validate("smoke", &p).is_ok());
        p.junit = Some("../old.xml".into());
        assert!(validate("smoke", &p).is_err());
        assert!(serde_json::from_value::<Profile>(json!({"platforms":{},"timout":3})).is_err());
        assert!(!tests_pass(
            &json!({"tests":0,"failed":0,"errors":0,"warnings":[]})
        ));
        assert!(!tests_pass(
            &json!({"tests":1,"failed":0,"errors":0,"warnings":["mismatch"]})
        ));
        assert!(tests_pass(
            &json!({"tests":1,"failed":0,"errors":0,"warnings":[]})
        ));
    }
    #[test]
    fn lifecycle_needs_readiness_and_unique_http_identity() {
        let mut p: Profile = serde_json::from_value(
            json!({"platforms":{"all":{"command":"test","launch":"serve"}}}),
        )
        .unwrap();
        assert!(validate("app", &p).is_err());
        p.ready_http = Some(HttpReady {
            port: 8123,
            path: "/ready".into(),
            status: 200,
            contains: "{run}".into(),
        });
        assert!(validate("app", &p).is_err());
        p.platforms.get_mut("all").unwrap().launch = Some("serve --token {run}".into());
        assert!(validate("app", &p).is_ok());
        p.ready_http.as_mut().unwrap().path = "/ HTTP/1.0\r\nX: bad".into();
        assert!(validate("app", &p).is_err());
        p.ready_http = None;
        p.ready_window = Some("Own Window".into());
        p.headless = true;
        assert!(validate("app", &p).is_err());
    }
}
