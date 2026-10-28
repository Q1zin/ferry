//! Native Windows process/receipt regression. Uses only a private temporary home, no app or input.
#![cfg(windows)]
use ferry_core::{
    desk::Shell,
    device::Dir,
    jobs::{
        evidence::{self, Action, Query},
        execution::{self, Resource, StartRequest, Started},
    },
};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

struct Fixture {
    home: PathBuf,
    runs: Vec<Started>,
}
impl Fixture {
    fn new() -> Self {
        let home = std::env::temp_dir().join(format!(
            "ferry-windows-ci-{}",
            ferry_core::jobs::unique_id()
        ));
        fs::create_dir_all(home.join(".ferry")).unwrap();
        Self {
            home,
            runs: Vec::new(),
        }
    }
    fn root(&self) -> PathBuf {
        self.home.join(".ferry")
    }
    fn start(&mut self, request: StartRequest) -> Started {
        let started = execution::start_at(
            &self.root(),
            request,
            &self.home,
            &[(
                "USERPROFILE".into(),
                self.home.to_string_lossy().into_owned(),
            )],
            Path::new(env!("CARGO_BIN_EXE_ferry")),
        )
        .unwrap();
        self.runs.push(started.clone());
        started
    }
    fn wait(&self, run: &Started) -> Value {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let value = evidence::execute(
                &self.root(),
                Query {
                    name: run.name.clone(),
                    run_id: Some(run.run_id.clone()),
                    action: Action::Result {
                        cursor: None,
                        max_bytes: 4096,
                    },
                },
            )
            .unwrap();
            let result = &value;
            if result["state"] != "running" {
                return result.clone();
            }
            assert!(Instant::now() < deadline, "job did not terminate: {value}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for run in &self.runs {
            let _ = execution::cancel_owned(&self.root(), &run.request_id, "ci");
        }
        let _ = fs::remove_dir_all(&self.home);
    }
}
fn request(name: &str, command: &str, timeout: u32) -> StartRequest {
    StartRequest {
        request_id: name.into(),
        name: name.into(),
        command: command.into(),
        dir: Dir::Home,
        shell: Shell::Powershell,
        resource: Resource::Headless,
        source: None,
        session: "ci".into(),
        keep: false,
        timeout_sec: Some(timeout),
    }
}

#[test]
fn powershell_receipt_survives_terminal_state_and_timeout_stops_job() {
    let mut fixture = Fixture::new();
    let launch_request = request("exit", "Write-Output 'NATIVE_WINDOWS_OK'; exit 7", 20);
    let first = fixture.start(launch_request.clone());
    let result = fixture.wait(&first);
    assert_eq!(result["exit_code"], 7, "{result}");
    assert!(result.to_string().contains("NATIVE_WINDOWS_OK"));
    let repeated = fixture.start(launch_request.clone());
    assert_eq!(first.run_id, repeated.run_id);
    assert!(repeated.reused);
    let mut conflict = launch_request;
    conflict.command = "Write-Output 'MUST_NOT_RUN'".into();
    assert!(
        execution::start_at(
            &fixture.root(),
            conflict,
            &fixture.home,
            &[],
            Path::new(env!("CARGO_BIN_EXE_ferry"))
        )
        .is_err()
    );
    let timed = fixture.start(request("timeout", "Start-Sleep -Seconds 60", 1));
    let result = fixture.wait(&timed);
    assert_ne!(result["state"], "succeeded", "{result}");
    assert_ne!(result["state"], "running", "{result}");
}
