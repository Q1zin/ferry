//! Declarative guarded UI steps. No retries, implicit focus, process launch or shell commands.
use crate::{checks, devices::finish, diagnostics};
use ferry_core::{
    control::{self, Request, Response},
    device::Fail,
    screen::{
        act::{ActionV2, Assertion, RequestV2, Snapshot},
        elements::{Locator, Tree},
        observe,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Path, PathBuf},
};

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Workflow {
    schema_version: u32,
    name: String,
    #[serde(default)]
    parameters: BTreeMap<String, Option<String>>,
    #[serde(default)]
    preconditions: Vec<Assertion>,
    steps: Vec<Step>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Action {
    Click(Locator),
    Key(String),
    Type(String),
}
impl Action {
    fn core(&self) -> ActionV2 {
        match self {
            Self::Click(v) => ActionV2::Click(v.clone()),
            Self::Key(v) => ActionV2::Key(v.clone()),
            Self::Type(v) => ActionV2::Type(v.clone()),
        }
    }
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Step {
    id: String,
    #[serde(default)]
    preconditions: Vec<Assertion>,
    action: Option<Action>,
    target: Option<Locator>,
    expect: Assertion,
    new_window: Option<String>,
    #[serde(default)]
    allow_already: bool,
    #[serde(default = "timeout")]
    timeout: u32,
}
fn timeout() -> u32 {
    10
}
fn parameter_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}
fn expand(value: &mut Value, parameters: &BTreeMap<String, String>) -> Result<(), Fail> {
    match value {
        Value::String(text) => {
            let mut result = String::new();
            let mut tail = text.as_str();
            while let Some(start) = tail.find("${") {
                result.push_str(&tail[..start]);
                let remaining = &tail[start + 2..];
                let end = remaining
                    .find('}')
                    .ok_or_else(|| Fail::new("незакрытый ${parameter}"))?;
                let name = &remaining[..end];
                result.push_str(
                    parameters
                        .get(name)
                        .ok_or_else(|| Fail::new(format!("не задан параметр {name}")))?,
                );
                tail = &remaining[end + 1..];
            }
            result.push_str(tail);
            if result.len() > 16384 {
                return Err(Fail::new("подстановка больше 16 КиБ"));
            }
            *text = result;
        }
        Value::Array(values) => {
            for v in values {
                expand(v, parameters)?;
            }
        }
        Value::Object(map) => {
            for (_, v) in map {
                expand(v, parameters)?;
            }
        }
        _ => {}
    }
    Ok(())
}
fn load(file: &Path, values: &BTreeMap<String, String>) -> Result<Workflow, Fail> {
    if fs::metadata(file)?.len() > 128 * 1024 {
        return Err(Fail::new("workflow больше 128 КиБ"));
    }
    let mut value: Value = serde_json::from_slice(&fs::read(file)?).map_err(Fail::new)?;
    let declared: BTreeMap<String, Option<String>> =
        serde_json::from_value(value.get("parameters").cloned().unwrap_or(json!({})))
            .map_err(Fail::new)?;
    if declared.len() > 32
        || declared
            .keys()
            .chain(values.keys())
            .any(|k| !parameter_name(k))
    {
        return Err(Fail::new("до 32 параметров, имена ASCII буквы/цифры/_"));
    }
    if values.keys().any(|k| !declared.contains_key(k)) {
        return Err(Fail::new("передан необъявленный параметр"));
    }
    let parameters = declared
        .into_iter()
        .map(|(k, v)| {
            let value = values
                .get(&k)
                .cloned()
                .or(v)
                .ok_or_else(|| Fail::new(format!("нужен --param {k}=VALUE")))?;
            if value.len() > 4096 {
                return Err(Fail::new("параметр больше 4096 байт"));
            }
            Ok((k, value))
        })
        .collect::<Result<BTreeMap<_, _>, Fail>>()?;
    // Definitions are metadata, not recursively expanded. Values are substituted once, without evaluation.
    value
        .as_object_mut()
        .ok_or_else(|| Fail::new("workflow должен быть объектом"))?
        .remove("parameters");
    expand(&mut value, &parameters)?;
    let workflow: Workflow = serde_json::from_value(value).map_err(Fail::new)?;
    validate(&workflow)?;
    Ok(workflow)
}
fn validate(w: &Workflow) -> Result<(), Fail> {
    if w.schema_version != 1
        || w.name.trim().is_empty()
        || w.name.len() > 120
        || w.steps.is_empty()
        || w.steps.len() > 100
        || w.preconditions.len() > 32
    {
        return Err(Fail::new(
            "schema_version=1, name 1..120, steps 1..100, preconditions <=32",
        ));
    }
    let mut ids = BTreeSet::new();
    for a in &w.preconditions {
        a.validate()?;
    }
    for s in &w.steps {
        if !parameter_name(&s.id)
            || !ids.insert(&s.id)
            || s.timeout > 30
            || s.preconditions.len() > 32
        {
            return Err(Fail::new(
                "уникальный step id, timeout 0..30, preconditions <=32",
            ));
        }
        s.expect.validate()?;
        for a in &s.preconditions {
            a.validate()?;
        }
        if let Some(t) = &s.target {
            t.validate()?;
        }
        match &s.action {
            Some(Action::Click(locator)) => locator.validate()?,
            Some(Action::Type(text) | Action::Key(text)) => {
                if text.is_empty() || text.len() > 4096 || s.target.is_none() {
                    return Err(Fail::new("type/key требуют target и текст 1..4096"));
                }
            }
            None if s.new_window.is_some() => return Err(Fail::new("new_window требует действие")),
            _ => {}
        }
        if s.new_window
            .as_ref()
            .is_some_and(|s| s.is_empty() || s.len() > 256)
        {
            return Err(Fail::new("new_window: точный заголовок 1..256"));
        }
    }
    Ok(())
}

trait Backend {
    fn observe(&mut self, title: &str, index: usize) -> Result<Snapshot, Fail>;
    fn act(&mut self, request: RequestV2, index: usize) -> Result<Value, Fail>;
    fn save(&mut self, value: &Value) -> Result<(), Fail>;
}
fn require(snapshot: &Snapshot, assertions: &[Assertion]) -> Result<(), Fail> {
    let window = serde_json::to_value(&snapshot.window).map_err(Fail::new)?;
    if !snapshot.complete
        || snapshot.changed_during_capture
        || window["focused"] != true
        || window["rect"].is_null()
    {
        return Err(Fail::new(
            "нужно полное стабильное наблюдение своего окна с фокусом",
        ));
    }
    let tree = Tree {
        elements: snapshot.elements.clone(),
        complete: snapshot.complete,
    };
    for condition in assertions {
        if !condition.evaluate(&tree)? {
            return Err(Fail::new("предусловие не выполнено; ввод не выполнялся"));
        }
    }
    Ok(())
}
fn execute(
    backend: &mut impl Backend,
    w: &Workflow,
    title: &str,
    dry_run: bool,
) -> Result<Value, Fail> {
    let mut title = title.to_string();
    let mut result = json!({"schema_version":1,"workflow":w.name,"state":"running","dry_run":dry_run,"started":ferry_core::now(),"steps":[],"automatic_retry":false});
    backend.save(&result)?;
    for (index, step) in w.steps.iter().enumerate() {
        let mut row = json!({"id":step.id,"state":"observing","performed":false});
        result["steps"].as_array_mut().unwrap().push(row.clone());
        backend.save(&result)?;
        let attempt = (|| {
            let snapshot = backend.observe(&title, index)?;
            require(&snapshot, if index == 0 { &w.preconditions } else { &[] })?;
            require(&snapshot, &step.preconditions)?;
            if let Some(action) = &step.action {
                row["state"] = "action_pending".into();
                // A lost response can mean the action happened. Never automatically replay it.
                row["outcome_unknown"] = true.into();
                result["steps"][index] = row.clone();
                backend.save(&result)?;
                let value = backend.act(
                    RequestV2 {
                        snapshot,
                        action: action.core(),
                        target: step.target.clone(),
                        expect: Some(step.expect.clone()),
                        new_window: step.new_window.clone(),
                        allow_already: step.allow_already,
                        timeout: step.timeout,
                        dry_run,
                        screenshot: true,
                    },
                    index,
                )?;
                row["action"] = value.clone();
                row["performed"] = value["performed"].clone();
                row["outcome_unknown"] = false.into();
                if value["error"].is_string() || (!dry_run && value["verified"] != true) {
                    return Err(Fail::new(
                        "действие не подтверждено; остановка без повторного ввода",
                    ));
                }
                if let Some(next) = &step.new_window {
                    title = next.clone();
                }
            } else {
                require(&snapshot, std::slice::from_ref(&step.expect))?;
            }
            Ok::<_, Fail>(())
        })();
        match attempt {
            Ok(()) => row["state"] = if dry_run { "planned" } else { "passed" }.into(),
            Err(e) => {
                row["state"] = "failed".into();
                row["error"] = e.to_string().into();
                result["state"] = "failed".into();
            }
        }
        row["finished_at"] = ferry_core::now().into();
        result["steps"][index] = row;
        backend.save(&result)?;
        if result["state"] == "failed" {
            break;
        }
    }
    if result["state"] == "running" {
        result["state"] = if dry_run { "dry_run" } else { "succeeded" }.into();
    }
    result["finished_at"] = ferry_core::now().into();
    backend.save(&result)?;
    Ok(result)
}

struct Native {
    device: Option<String>,
    job: String,
    run: String,
    session: String,
    root: PathBuf,
}
impl Native {
    fn call(&self, request: Request) -> Result<Value, Fail> {
        match checks::send(request, &self.session)? {
            Response::Report(v) => serde_json::from_str(&v).map_err(Fail::new),
            _ => Err(Fail::new("нет JSON ответа")),
        }
    }
}
impl Backend for Native {
    fn observe(&mut self, title: &str, index: usize) -> Result<Snapshot, Fail> {
        let ownership = diagnostics::evidence(
            &self.device,
            &self.job,
            &self.run,
            ferry_core::jobs::evidence::Action::Probe {
                http: None,
                window: Some(title.into()),
            },
        )?;
        if ownership["running"] != true
            || ownership["windows"].as_array().is_none_or(|w| w.len() != 1)
        {
            return Err(Fail::new("нужно одно точное окно указанного живого run_id"));
        }
        let window = &ownership["windows"][0];
        let id = window["id"]
            .as_i64()
            .ok_or_else(|| Fail::new("нет window id"))?;
        let value = self.call(Request::ObserveV2 {
            device: self.device.clone(),
            request: observe::RequestV2 {
                window: format!("@{id}"),
                ocr: false,
                screenshot: true,
                query: None,
                subtree: false,
            },
            file: self.root.join(format!("step-{index}-before.png")),
        })?;
        let snapshot = Snapshot {
            window: serde_json::from_value(value["window"].clone()).map_err(Fail::new)?,
            captured_at: value["captured_at"]
                .as_u64()
                .ok_or_else(|| Fail::new("нет captured_at"))?,
            elements: serde_json::from_value(value["elements"].clone()).map_err(Fail::new)?,
            complete: value["complete"] == true,
            changed_during_capture: value["changed_during_capture"] != false,
        };
        if value["window"]["id"] != window["id"]
            || value["window"]["pid"] != window["pid"]
            || value["window"]["title"] != title
        {
            return Err(Fail::new("своё окно изменилось во время наблюдения"));
        }
        ferry_core::write_private(
            &self.root.join(format!("step-{index}-before.json")),
            &value.to_string(),
        )?;
        Ok(snapshot)
    }
    fn act(&mut self, request: RequestV2, index: usize) -> Result<Value, Fail> {
        let expected = serde_json::to_value(&request.snapshot.window).map_err(Fail::new)?;
        let ownership = diagnostics::evidence(
            &self.device,
            &self.job,
            &self.run,
            ferry_core::jobs::evidence::Action::Probe {
                http: None,
                window: Some(
                    expected["title"]
                        .as_str()
                        .ok_or_else(|| Fail::new("нет заголовка своего окна"))?
                        .into(),
                ),
            },
        )?;
        if ownership["running"] != true
            || ownership["windows"].as_array().is_none_or(|windows| {
                windows.len() != 1
                    || windows[0]["id"] != expected["id"]
                    || windows[0]["pid"] != expected["pid"]
            })
        {
            return Err(Fail::new("свой run/окно изменились перед вводом"));
        }
        self.call(Request::ActV2 {
            device: self.device.clone(),
            request: Box::new(request),
            file: self.root.join(format!("step-{index}-after.png")),
        })
    }
    fn save(&mut self, value: &Value) -> Result<(), Fail> {
        ferry_core::write_private(
            &self.root.join("report.json"),
            &serde_json::to_string_pretty(value).map_err(Fail::new)?,
        )?;
        Ok(())
    }
}

pub fn run(args: &[&str]) -> io::Result<i32> {
    let [action @ ("validate" | "run"), file, tail @ ..] = args else {
        return crate::usage();
    };
    let (mut rest, mut device, mut job, mut run, mut window, mut dry, mut parameters) =
        (tail, None, None, None, None, false, BTreeMap::new());
    while !rest.is_empty() {
        match rest {
            ["--param", value, tail @ ..] => {
                let (key, value) = value
                    .split_once('=')
                    .ok_or_else(|| io::Error::other("--param NAME=VALUE"))?;
                if parameters.insert(key.into(), value.into()).is_some() {
                    return Err(io::Error::other("параметр указан дважды"));
                }
                rest = tail;
            }
            ["--device", v, tail @ ..] => {
                device = Some(*v);
                rest = tail;
            }
            ["--job", v, tail @ ..] => {
                job = Some(*v);
                rest = tail;
            }
            ["--run", v, tail @ ..] => {
                run = Some(*v);
                rest = tail;
            }
            ["--window", v, tail @ ..] => {
                window = Some(*v);
                rest = tail;
            }
            ["--dry-run", tail @ ..] => {
                dry = true;
                rest = tail;
            }
            _ => return crate::usage(),
        }
    }
    finish((|| {
        let workflow = load(Path::new(file), &parameters)?;
        if *action == "validate" {
            println!(
                "{}",
                json!({"valid":true,"name":workflow.name,"steps":workflow.steps.len(),"schema_version":1,"executed":false})
            );
            return Ok(0);
        }
        let (job, run, title) = (
            job.ok_or_else(|| Fail::new("нужен --job"))?,
            run.ok_or_else(|| Fail::new("нужен точный --run"))?,
            window.ok_or_else(|| Fail::new("нужен --window с точным заголовком"))?,
        );
        if title.is_empty() || title.len() > 256 {
            return Err(Fail::new("window: 1..256 байт"));
        }
        let root = ferry_core::ferry_home()?
            .join("workflows")
            .join(ferry_core::jobs::unique_id());
        fs::create_dir_all(&root)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        }
        let worker = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join("worker.lock"))?;
        worker.lock()?;
        let mut backend = Native {
            device: device.filter(|d| *d != "local").map(String::from),
            job: job.into(),
            run: run.into(),
            session: control::session(),
            root: root.clone(),
        };
        let mut result = execute(&mut backend, &workflow, title, dry)?;
        result["report_file"] = root
            .join("report.json")
            .to_string_lossy()
            .to_string()
            .into();
        result["device"] = device.unwrap_or("local").into();
        result["job"] = job.into();
        result["run_id"] = run.into();
        backend.save(&result)?;
        println!("{result}");
        Ok(i32::from(result["state"] == "failed"))
    })())
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture {
        snapshot: Snapshot,
        calls: usize,
        fail: bool,
        reports: Vec<Value>,
    }
    impl Backend for Fixture {
        fn observe(&mut self, _: &str, _: usize) -> Result<Snapshot, Fail> {
            Ok(self.snapshot.clone())
        }
        fn act(&mut self, r: RequestV2, _: usize) -> Result<Value, Fail> {
            self.calls += 1;
            assert!(r.dry_run, "fixtures never perform real input");
            if self.fail {
                Err(Fail::new("lost response"))
            } else {
                Ok(json!({"performed":false,"verified":false,"dry_run":true,"error":null}))
            }
        }
        fn save(&mut self, v: &Value) -> Result<(), Fail> {
            self.reports.push(v.clone());
            Ok(())
        }
    }
    fn fixture() -> Fixture {
        let snapshot:Snapshot=serde_json::from_value(json!({"window":{"id":1,"pid":10,"process":"fixture","title":"Own","focused":true,"rect":[0,0,100,100]},"captured_at":ferry_core::now(),"complete":true,"changed_during_capture":false,"elements":[{"index":1,"parent":null,"role":"button","name":"Save","id":"save","rect":[1,1,10,10],"enabled":true,"focused":true,"checked":null,"selected":null,"value":null,"protected":false}]})).unwrap();
        Fixture {
            snapshot,
            calls: 0,
            fail: false,
            reports: Vec::new(),
        }
    }
    fn workflow() -> Workflow {
        serde_json::from_value(json!({"schema_version":1,"name":"fixture","steps":[{"id":"save","action":{"click":{"name":"Save"}},"expect":{"locator":{"name":"Done"},"property":"exists"}},{"id":"save_again","action":{"click":{"name":"Save"}},"expect":{"locator":{"name":"Done"},"property":"exists"}}]})).unwrap()
    }
    #[test]
    fn lost_action_reply_is_durable_and_never_retried() {
        let mut f = fixture();
        f.fail = true;
        let r = execute(&mut f, &workflow(), "Own", true).unwrap();
        assert_eq!(f.calls, 1);
        assert_eq!(r["state"], "failed");
        assert_eq!(r["steps"][0]["outcome_unknown"], true);
        assert!(
            f.reports
                .iter()
                .any(|r| r["steps"][0]["state"] == "action_pending")
        );
    }
    #[test]
    fn failed_preconditions_prevent_every_action() {
        let mut f = fixture();
        f.snapshot.complete = false;
        let r = execute(&mut f, &workflow(), "Own", true).unwrap();
        assert_eq!(f.calls, 0);
        assert_eq!(r["state"], "failed");
    }
    #[test]
    fn dry_run_never_claims_asserted_success() {
        let mut f = fixture();
        let r = execute(&mut f, &workflow(), "Own", true).unwrap();
        assert_eq!(f.calls, 2);
        assert_eq!(r["state"], "dry_run");
        assert_eq!(r["steps"][0]["state"], "planned");
    }
    #[test]
    fn substitution_is_literal_and_missing_parameters_fail() {
        let mut v = json!({"text":"${name}"});
        expand(
            &mut v,
            &BTreeMap::from([("name".into(), "${other}\";rm -rf /".into())]),
        )
        .unwrap();
        assert_eq!(v["text"], "${other}\";rm -rf /");
        assert!(expand(&mut json!("${missing}"), &BTreeMap::new()).is_err());
    }
}
