//! Доказательства одного запуска; недоступные источники остаются явными ошибками.
use crate::{
    agent_tools::{report, shot_path},
    devices::{ask, finish},
};
use ferry_core::{
    control::{Request, Response},
    device::Fail,
    jobs::evidence::{Action, Query},
};
use serde_json::{Value, json};
use std::{io, path::PathBuf};

pub(crate) fn evidence(
    device: &Option<String>,
    name: &str,
    run: &str,
    action: Action,
) -> Result<Value, Fail> {
    report(Request::Evidence {
        device: device.clone(),
        query: Query {
            name: name.into(),
            run_id: Some(run.into()),
            action,
        },
    })
}
fn partial(result: Result<Value, Fail>) -> Value {
    result.unwrap_or_else(|e| json!({"error":e.to_string()}))
}
pub(crate) fn collect(
    device: &Option<String>,
    name: &str,
    run: &str,
    window: Option<&str>,
    junit: Option<&str>,
    session: Option<&str>,
) -> Value {
    let ask = |r| match session {
        Some(s) => crate::checks::send(r, s),
        None => ask(r),
    };
    let result = partial(evidence(
        device,
        name,
        run,
        Action::Result {
            cursor: None,
            max_bytes: 8192,
        },
    ));
    let files = partial(evidence(device, name, run, Action::List));
    let info = partial(match device {
        Some(d) => ask(Request::Info { device: d.clone() }).and_then(|r| match r {
            Response::Info(v) => Ok(json!(v)),
            _ => Err(Fail::new("нет сведений системы")),
        }),
        None => ferry_core::info::collect()
            .map(|v| json!(v))
            .map_err(Fail::from),
    });
    let mut value = json!({"device":device.as_deref().unwrap_or("local"),"name":name,"run_id":run,"collected_at":ferry_core::now(),"result":result,"evidence":files,"environment":info,"controller_version":ferry_core::VERSION});
    value["tool_versions"] = partial(evidence(
        device,
        name,
        run,
        Action::Versions {
            shell: ferry_core::desk::Shell::Default,
            tools: ["git", "cargo", "rustc", "node", "bun", "python3"]
                .map(String::from)
                .to_vec(),
        },
    ));
    if let Some(id) = junit {
        value["tests"] = partial(evidence(
            device,
            name,
            run,
            Action::Tests {
                artifact: id.into(),
            },
        ));
    }
    if let Some(w) = window {
        value["observation"] = partial((|| {
            let r = ask(Request::Observe {
                device: device.clone(),
                request: ferry_core::screen::observe::Request {
                    window: w.into(),
                    ocr: false,
                },
                file: shot_path()?,
            })?;
            match r {
                Response::Report(s) => {
                    serde_json::from_str(&s).map_err(|e| Fail::new(e.to_string()))
                }
                _ => Err(Fail::new("нет наблюдения")),
            }
        })());
        let crash = match device {
            Some(d) => ask(Request::Crashes {
                device: d.clone(),
                process: w.into(),
                last: true,
            })
            .and_then(|r| match r {
                Response::Message(s) => Ok(s),
                _ => Err(Fail::new("нет отчёта падения")),
            }),
            None => ferry_core::crashes::report(w, true).map_err(Fail::from),
        };
        value["crashes"]=partial(crash.map(|s|json!({"text":s.chars().take(4096).collect::<String>(),"truncated":s.chars().count()>4096,"note":"Последний отчёт по фильтру; связь с этим запуском не доказана"})));
    }
    value
}
pub(crate) fn save(value: &Value, folder: &str) -> Result<PathBuf, Fail> {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dir = ferry_core::ferry_home()?
        .join(folder)
        .join(format!("{}-{nonce}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let mut value = value.clone();
    if let Some(source) = value["observation"]["screenshot"].as_str() {
        let target = dir.join("window.png");
        std::fs::copy(source, &target)?;
        value["observation"]["screenshot"] = target.to_string_lossy().into_owned().into();
    }
    let file = dir.join("report.json");
    ferry_core::write_private(
        &file,
        &serde_json::to_string_pretty(&value).map_err(|e| Fail::new(e.to_string()))?,
    )?;
    Ok(file)
}
pub fn run(args: &[&str]) -> io::Result<i32> {
    let (device, name, mut rest) = match args {
        [d, n, tail @ ..] if !n.starts_with('-') => (Some((*d).to_string()), *n, tail),
        [n, tail @ ..] => (None, *n, tail),
        _ => return crate::usage(),
    };
    let (mut run, mut window, mut junit) = (None, None, None);
    while !rest.is_empty() {
        match rest {
            ["--run", id, tail @ ..] => {
                run = Some(*id);
                rest = tail;
            }
            ["--window", w, tail @ ..] => {
                window = Some(*w);
                rest = tail;
            }
            ["--junit", id, tail @ ..] => {
                junit = Some(*id);
                rest = tail;
            }
            _ => return crate::usage(),
        }
    }
    let Some(run) = run else {
        return crate::usage();
    };
    finish((|| {
        let mut value = collect(&device, name, run, window, junit, None);
        let file = save(&value, "diagnostics")?;
        value["report_file"] = file.to_string_lossy().into_owned().into();
        let failed = value["result"]["error"].is_string();
        println!("{value}");
        Ok(i32::from(failed))
    })())
}
