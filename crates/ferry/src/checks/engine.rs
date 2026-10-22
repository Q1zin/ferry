use super::*;
use ferry_core::{
    device::Dir,
    jobs::execution::{Resource, Source, StartRequest, Started},
};
use journal::{Operation, Phase, Target};
use std::path::Path;

fn report_response(response: Response) -> Result<Value, Fail> {
    match response {
        Response::Report(s) => serde_json::from_str(&s).map_err(Fail::new),
        _ => Err(Fail::new("нет структурированного ответа")),
    }
}
fn save(root: &Path, index: usize, target: &Target) -> Result<(), Fail> {
    journal::write(&journal::target_path(root, index), target)
}
fn cancelled(root: &Path) -> bool {
    root.join("cancel").exists()
}
fn error(target: &mut Target, message: impl ToString) {
    if !target.row["errors"].is_array() {
        target.row["errors"] = json!([]);
    }
    target.row["errors"]
        .as_array_mut()
        .unwrap()
        .push(message.to_string().into());
}
fn evidence(device: &Option<String>, phase: &Phase, action: Action) -> Result<Value, Fail> {
    diagnostics::evidence(
        device,
        &phase.job,
        phase
            .run_id
            .as_deref()
            .ok_or_else(|| Fail::new("нет run_id"))?,
        action,
    )
}
fn cancel_phase(
    device: &Option<String>,
    phase: &Phase,
    session: &str,
) -> Result<Option<Started>, Fail> {
    let value = report_response(send(
        Request::ExecutionCancel {
            device: device.clone(),
            request_id: phase.request_id.clone(),
        },
        session,
    )?)?;
    // The server tombstone also cancels a delayed start whose reply was lost.
    if value["cancelled"] != true {
        return Err(Fail::new("отмена не подтверждена"));
    }
    if value["started"].is_null() {
        Ok(None)
    } else {
        serde_json::from_value(value["started"].clone())
            .map(Some)
            .map_err(Fail::new)
    }
}

struct Run<'a> {
    root: &'a Path,
    op: &'a Operation,
    index: usize,
    device: Option<String>,
    session: String,
    spec: Spec,
    target: Target,
    collect_only: bool,
}
impl Run<'_> {
    fn save(&self) -> Result<(), Fail> {
        save(self.root, self.index, &self.target)
    }
    fn token(&self) -> String {
        format!("{}-{}", self.op.id, self.index)
    }
    fn output(&self) -> String {
        format!(".ferry-checks/{}", self.token())
    }
    fn expired(&self) -> bool {
        self.target.deadline.is_some_and(|d| ferry_core::now() >= d)
    }
    fn stopped(&self) -> bool {
        cancelled(self.root) || self.expired()
    }
    fn prepare(&mut self, status: &Status) -> Result<(), Fail> {
        self.target.row["errors"] = json!([]);
        self.target.row["status"] = json!(status);
        self.target.row["capabilities"] = preflight(
            &self.device,
            status,
            &self.spec,
            &self.op.profile,
            &self.session,
        )?;
        if !self.op.profile.headless {
            // Persist intent before taking the lease so a killed worker can release it.
            self.target.lease = true;
            self.save()?;
            send(
                Request::Desk {
                    device: self.device.as_deref().unwrap_or("local").into(),
                    action: "take".into(),
                    minutes: self.op.profile.timeout / 60 + 12,
                },
                &self.session,
            )?;
        }
        if !self.target.synchronized {
            if let Some(project) = &self.op.project {
                if self.op.isolated {
                    let (commit, tree) = project
                        .snapshot
                        .as_ref()
                        .ok_or_else(|| Fail::new("нет сохранённого снимка"))?;
                    let value = report_response(send(
                        Request::WorkspaceSync {
                            device: self.device.clone(),
                            root: project.root.clone(),
                            project: project.name.clone(),
                            id: self.token(),
                            commit: commit.clone(),
                            tree: tree.clone(),
                        },
                        &self.session,
                    )?)?;
                    self.target.source = Some(value["source"].clone());
                } else if let (Some(device), Some((commit, tree))) =
                    (&self.device, &project.snapshot)
                {
                    send(
                        Request::Sync {
                            device: device.clone(),
                            root: project.root.clone(),
                            project: project.name.clone(),
                            commit: commit.clone(),
                            tree: tree.clone(),
                        },
                        &self.session,
                    )?;
                }
            }
            self.target.synchronized = true;
            self.save()?;
        }
        if self.target.deadline.is_none() {
            self.target.deadline = Some(ferry_core::now() + u64::from(self.op.profile.timeout));
        }
        self.target.state = "running".into();
        self.save()
    }
    fn phase(&mut self, name: &str, cmd: &str) -> Result<usize, Fail> {
        let index = if let Some(i) = self.target.phases.iter().position(|p| p.name == name) {
            i
        } else {
            if self.collect_only {
                return Err(Fail::new(
                    "collect не запускает новые фазы; продолжить: check resume ID",
                ));
            }
            let id = format!("{}-{}-{name}", self.op.id, self.index);
            self.target.phases.push(Phase {
                name: name.into(),
                job: format!("check-{id}"),
                request_id: id,
                started: ferry_core::now(),
                ready_log: self.op.profile.ready_log.is_none(),
                ..Phase::default()
            });
            self.target.cleanup_pending = true;
            self.save()?;
            self.target.phases.len() - 1
        };
        if self.collect_only && self.target.phases[index].run_id.is_none() {
            let value = report_response(send(
                Request::ExecutionLookup {
                    device: self.device.clone(),
                    request_id: self.target.phases[index].request_id.clone(),
                },
                &self.session,
            )?)?;
            let started: Started =
                serde_json::from_value(value["started"].clone()).map_err(|_| {
                    Fail::new("старт не подтверждён; collect не повторяет запуск, используй resume")
                })?;
            self.target.phases[index].run_id = Some(started.run_id);
            self.save()?;
        }
        if self.target.phases[index].run_id.is_none() {
            if self.stopped() {
                return Ok(index);
            }
            let source = self
                .target
                .source
                .clone()
                .map(serde_json::from_value::<Source>)
                .transpose()
                .map_err(Fail::new)?;
            let dir = match (&self.op.project, &source) {
                (Some(project), Some(source)) => Dir::Project {
                    name: source.workspace.clone(),
                    prefix: project.prefix.clone(),
                },
                (Some(project), None) => Dir::Project {
                    name: project.name.clone(),
                    prefix: project.prefix.clone(),
                },
                _ => Dir::Home,
            };
            let mut spec = self.spec.clone();
            spec.command = cmd.replace("{run}", &self.token());
            let phase = &self.target.phases[index];
            let request = StartRequest {
                request_id: phase.request_id.clone(),
                name: phase.job.clone(),
                command: command(&spec, &self.output()),
                dir,
                shell: spec.shell,
                resource: if self.op.profile.headless {
                    Resource::Headless
                } else {
                    Resource::Desktop
                },
                source,
                session: self.session.clone(),
                keep: true,
                timeout_sec: Some(self.op.profile.timeout),
            };
            let value = report_response(send(
                Request::AtomicStart {
                    device: self.device.clone(),
                    request,
                    local_cwd: (self.device.is_none() && !self.op.isolated)
                        .then(|| self.op.cwd.clone()),
                },
                &self.session,
            )?)?;
            let started: Started = serde_json::from_value(value).map_err(Fail::new)?;
            self.target.phases[index].run_id = Some(started.run_id);
            self.target.row["job"] = started.name.into();
            self.target.row["run_id"] = json!(self.target.phases[index].run_id);
            self.save()?;
        }
        Ok(index)
    }
    fn poll(&mut self, index: usize, readiness: bool) -> Result<Value, Fail> {
        let token = self.token();
        let p = &self.target.phases[index];
        let value = evidence(
            &self.device,
            p,
            Action::Result {
                cursor: p.cursor.clone(),
                max_bytes: 16384,
            },
        )?;
        let phase = &mut self.target.phases[index];
        phase.cursor = value["cursor"].as_str().map(str::to_owned);
        phase
            .carry
            .push_str(value["log"].as_str().unwrap_or_default());
        if readiness {
            phase.ready_log |= self
                .op
                .profile
                .ready_log
                .as_ref()
                .is_none_or(|needle| phase.carry.contains(&needle.replace("{run}", &token)));
        }
        if phase.carry.len() > 1024 {
            let mut offset = phase.carry.len() - 1024;
            while !phase.carry.is_char_boundary(offset) {
                offset += 1;
            }
            phase.carry = phase.carry[offset..].into();
        }
        if value["state"] != "running" {
            phase.result = Some(value.clone());
            phase.finished_seen.get_or_insert_with(ferry_core::now);
        }
        self.save()?;
        Ok(value)
    }
    fn ready(&mut self, index: usize) -> Result<bool, Fail> {
        let p = &self.op.profile;
        if !self.target.phases[index].ready_probe {
            let mut ready = true;
            if let Some(port) = p.ready_port {
                ready &= match &self.device {
                    Some(d) => matches!(
                        send(
                            Request::PortOpen {
                                device: d.clone(),
                                port
                            },
                            &self.session
                        )?,
                        Response::Flag(true)
                    ),
                    None => std::net::TcpStream::connect_timeout(
                        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
                        Duration::from_millis(100),
                    )
                    .is_ok(),
                };
            }
            if p.ready_http.is_some() || p.ready_window.is_some() {
                let http = p.ready_http.as_ref().map(|h| jobs::probe::HttpProbe {
                    port: h.port,
                    path: h.path.replace("{run}", &self.token()),
                    status: h.status,
                    contains: h.contains.replace("{run}", &self.token()),
                });
                let value = evidence(
                    &self.device,
                    &self.target.phases[index],
                    Action::Probe {
                        http,
                        window: p
                            .ready_window
                            .as_ref()
                            .map(|w| w.replace("{run}", &self.token())),
                    },
                )?;
                ready &= value["ready"] == true;
                self.target.row["readiness_probe"] = value;
            }
            self.target.phases[index].ready_probe = ready;
        }
        let phase = &self.target.phases[index];
        let ready = phase.ready_log && phase.ready_probe;
        self.target.row["ready"] = json!({"log":phase.ready_log,"probe":phase.ready_probe});
        self.save()?;
        Ok(ready)
    }
    fn wait(&mut self, index: usize, launch: bool, readiness: bool) -> Result<bool, Fail> {
        loop {
            if cancelled(self.root) {
                return Ok(false);
            }
            if self.expired() {
                if !launch
                    && let Some(done) = completed_before_deadline(
                        &self.target.phases[index],
                        self.target.deadline.unwrap(),
                        readiness,
                        &self.op.profile,
                    )
                {
                    return Ok(done);
                }
                self.target.row["timed_out"] = true.into();
                self.save()?;
                return Ok(false);
            }
            let value = self.poll(index, readiness)?;
            let ready = !readiness || self.ready(index)?;
            if !launch && let Some(p) = self.target.phases.iter().find(|p| p.name == "launch") {
                let live = evidence(
                    &self.device,
                    p,
                    Action::Result {
                        cursor: None,
                        max_bytes: 4,
                    },
                )?;
                if live["state"] != "running" {
                    error(&mut self.target, "приложение завершилось во время проверки");
                    return Ok(false);
                }
            }
            if launch {
                if value["state"] != "running" {
                    error(&mut self.target, "launch завершился до окончания проверки");
                    return Ok(false);
                }
                if ready {
                    return Ok(true);
                }
            } else if value["state"] != "running" {
                if !ready {
                    error(&mut self.target, "условия готовности не выполнены");
                }
                return Ok(ready && value["state"] == "succeeded");
            }
            if value["more"] != true {
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
    fn cleanup(&mut self) -> Result<(), Fail> {
        let mut errors = Vec::new();
        for index in 0..self.target.phases.len() {
            let phase = &mut self.target.phases[index];
            let stopped = (|| -> Result<(), Fail> {
                if phase.run_id.is_none() {
                    if let Some(found) = cancel_phase(&self.device, phase, &self.session)? {
                        phase.run_id = Some(found.run_id);
                    } else {
                        phase.result = Some(json!({"state":"not_started"}));
                        return Ok(());
                    }
                }
                let mut value = evidence(
                    &self.device,
                    phase,
                    Action::Result {
                        cursor: None,
                        max_bytes: 8192,
                    },
                )?;
                if value["state"] == "running" {
                    cancel_phase(&self.device, phase, &self.session)?;
                    value = evidence(
                        &self.device,
                        phase,
                        Action::Result {
                            cursor: None,
                            max_bytes: 8192,
                        },
                    )?;
                    if value["state"] == "running" {
                        return Err(Fail::new("процесс ещё завершается"));
                    }
                }
                phase.result = Some(value);
                Ok(())
            })();
            if let Err(e) = stopped {
                errors.push(format!("cleanup {}: {e}", phase.name));
            }
            self.save()?;
        }
        if errors.is_empty() && self.target.lease {
            if let Err(e) = send(
                Request::Desk {
                    device: self.device.as_deref().unwrap_or("local").into(),
                    action: "release-owned".into(),
                    minutes: 0,
                },
                &self.session,
            ) {
                errors.push(format!("снять бронь: {e}"));
            } else {
                self.target.lease = false;
            }
        }
        self.target.cleanup_pending = !errors.is_empty();
        for e in errors {
            error(&mut self.target, e);
        }
        self.save()
    }
    fn collect(&mut self) -> Result<(), Fail> {
        let Some(phase) = self
            .target
            .phases
            .iter()
            .rev()
            .find(|p| p.run_id.is_some())
            .cloned()
        else {
            return Ok(());
        };
        self.target.row["job"] = phase.job.clone().into();
        self.target.row["run_id"] = json!(phase.run_id);
        self.target.row["result"] = evidence(
            &self.device,
            &phase,
            Action::Result {
                cursor: None,
                max_bytes: 8192,
            },
        )?;
        if phase.name == "test" {
            for path in self
                .op
                .profile
                .artifacts
                .iter()
                .chain(self.op.profile.junit.iter())
            {
                let existing = self.target.row["collected"].get(path).cloned();
                let artifact = if let Some(a) = existing {
                    a
                } else {
                    match evidence(
                        &self.device,
                        &phase,
                        Action::Add {
                            path: format!("{}/{path}", self.output()),
                        },
                    ) {
                        Ok(a) => {
                            if !self.target.row["collected"].is_object() {
                                self.target.row["collected"] = json!({});
                            }
                            self.target.row["collected"][path] = a["artifact"].clone();
                            self.save()?;
                            a["artifact"].clone()
                        }
                        Err(e) => {
                            error(&mut self.target, format!("артефакт {path}: {e}"));
                            continue;
                        }
                    }
                };
                if self.op.profile.junit.as_ref() == Some(path) {
                    let id = artifact["id"]
                        .as_str()
                        .ok_or_else(|| Fail::new("нет id JUnit"))?;
                    match evidence(
                        &self.device,
                        &phase,
                        Action::Tests {
                            artifact: id.into(),
                        },
                    ) {
                        Ok(t) => {
                            if !tests_pass(&t) {
                                error(
                                    &mut self.target,
                                    "JUnit: падения, пустой или неполный отчёт",
                                );
                            }
                            self.target.row["tests"] = t;
                        }
                        Err(e) => error(&mut self.target, format!("JUnit: {e}")),
                    }
                }
            }
            self.target.row["artifacts"] = self.target.row["collected"]
                .as_object()
                .map(|m| m.values().cloned().collect::<Vec<_>>())
                .unwrap_or_default()
                .into();
        }
        self.save()
    }
    fn finish(&mut self) -> Result<(), Fail> {
        if self.target.outcome.as_deref() == Some("succeeded") {
            self.target.row["errors"] = json!([]);
        }
        self.target.state = "collecting".into();
        self.target.cleanup_pending = true;
        self.save()?;
        if self.target.row["final_observation"].is_null() && !self.target.phases.is_empty() {
            let window = self.target.row["readiness_probe"]["windows"]
                .as_array()
                .and_then(|w| w.first())
                .and_then(|w| w["id"].as_u64())
                .map(|id| format!("@{id}"))
                .or_else(|| {
                    self.op
                        .profile
                        .window
                        .as_ref()
                        .map(|w| w.replace("{run}", &self.token()))
                });
            if let Some(window) = window {
                let observation = (|| -> Result<Value, Fail> {
                    report_response(send(
                        Request::Observe {
                            device: self.device.clone(),
                            request: ferry_core::screen::observe::Request { window, ocr: false },
                            file: crate::agent_tools::shot_path()?,
                        },
                        &self.session,
                    )?)
                })();
                self.target.row["final_observation"] =
                    observation.unwrap_or_else(|e| json!({"error":e.to_string()}));
                self.save()?;
            }
        }
        self.cleanup()?;
        if self.target.cleanup_pending {
            return Ok(());
        }
        self.collect()?;
        let mut outcome = self
            .target
            .outcome
            .clone()
            .unwrap_or_else(|| "failed".into());
        if self.target.row["errors"]
            .as_array()
            .is_some_and(|a| !a.is_empty())
            && outcome == "succeeded"
        {
            outcome = "failed".into();
        }
        if outcome != "succeeded"
            && let Some(phase) = self.target.phases.iter().rev().find(|p| p.run_id.is_some())
        {
            let run = phase.run_id.as_deref().unwrap();
            let junit = self.target.row["collected"]
                .get(self.op.profile.junit.as_deref().unwrap_or(""))
                .and_then(|a| a["id"].as_str());
            let mut diagnosis = diagnostics::collect(
                &self.device,
                &phase.job,
                run,
                None,
                junit,
                Some(&self.session),
            );
            if !self.target.row["final_observation"].is_null() {
                diagnosis["observation"] = self.target.row["final_observation"].clone();
            }
            match diagnostics::save(&diagnosis, "diagnostics") {
                Ok(file) => {
                    self.target.row["diagnostic_file"] = file.to_string_lossy().into_owned().into()
                }
                Err(e) => error(&mut self.target, format!("сохранение диагностики: {e}")),
            }
            self.target.row["diagnostics"] = diagnosis;
        }
        self.target.state = outcome;
        self.save()
    }
    fn execute(&mut self, status: Option<Status>) -> Result<(), Fail> {
        if journal::terminal(&self.target.state)
            && self.target.state != "unavailable"
            && !self.target.cleanup_pending
        {
            return Ok(());
        }
        if cancelled(self.root) {
            self.target.outcome = Some("cancelled".into());
            return self.finish();
        }
        if self.target.outcome.is_some() {
            return self.finish();
        }
        let status = status.ok_or_else(|| Fail::new("нет связи или статуса устройства"))?;
        if !self.collect_only {
            self.prepare(&status)?;
        }
        let mut ok = true;
        if let Some(build) = self.spec.build.clone() {
            let i = self.phase("build", &build)?;
            ok = self.wait(i, false, false)?;
        }
        if ok && let Some(launch) = self.spec.launch.clone() {
            let i = self.phase("launch", &launch)?;
            ok = self.wait(i, true, true)?;
        }
        if ok {
            let i = self.phase("test", &self.spec.command.clone())?;
            ok = self.wait(i, false, self.spec.launch.is_none())?;
        }
        if self.target.row["timed_out"] == true {
            error(
                &mut self.target,
                "превышен timeout; остановлены только запуски этой операции",
            );
            self.target.row["timed_out"] = true.into();
        }
        self.target.outcome = Some(
            if cancelled(self.root) {
                "cancelled"
            } else if ok {
                "succeeded"
            } else {
                "failed"
            }
            .into(),
        );
        self.finish()
    }
}
fn completed_before_deadline(
    phase: &Phase,
    deadline: u64,
    readiness: bool,
    profile: &Profile,
) -> Option<bool> {
    if !phase
        .finished_seen
        .is_some_and(|finished| finished <= deadline)
    {
        return None;
    }
    let result = phase.result.as_ref()?;
    let ready = !readiness
        || ((profile.ready_log.is_none() || phase.ready_log)
            && ((profile.ready_port.is_none()
                && profile.ready_http.is_none()
                && profile.ready_window.is_none())
                || phase.ready_probe));
    Some(result["state"] == "succeeded" && ready)
}

fn one(root: &Path, op: &Operation, index: usize, collect_only: bool) -> Result<(), Fail> {
    let (device, _) = &op.targets[index];
    let target = journal::target(root, op, index)?;
    let current = state()?;
    let status = match device {
        None => Some(current.me),
        Some(d) => current
            .find(d)
            .ok()
            .filter(|p| p.online)
            .and_then(|p| p.status.clone()),
    };
    if status.as_ref().is_some_and(|s| {
        !op.profile.platforms.contains_key(&s.os) && !op.profile.platforms.contains_key("all")
    }) && target.phases.is_empty()
    {
        let mut target = target;
        target.state = "unavailable".into();
        error(&mut target, "нет команды для этой платформы");
        save(root, index, &target)?;
        return Ok(());
    }
    let spec = status
        .as_ref()
        .and_then(|s| op.profile.platforms.get(&s.os))
        .or_else(|| op.profile.platforms.get("all"))
        .cloned()
        .or_else(|| op.profile.platforms.values().next().cloned())
        .ok_or_else(|| Fail::new("нет команды платформы"))?;
    let mut run = Run {
        root,
        op,
        index,
        device: device.clone(),
        session: format!("check:{}:{index}", op.id),
        spec,
        target,
        collect_only,
    };
    if let Err(e) = run.execute(status) {
        error(&mut run.target, e);
        // Evidence collection may fail after cleanup; resuming must retry collection.
        if !journal::terminal(&run.target.state) || run.target.state == "succeeded" {
            run.target.state = "incomplete".into();
        }
        run.save()?;
    }
    eprintln!(
        "ferry check [{}]: {}",
        device.as_deref().unwrap_or("local"),
        run.target.state
    );
    Ok(())
}

pub(super) fn worker(root: &Path, op: &Operation, collect_only: bool) -> Result<i32, Fail> {
    let guard = journal::lock(root)?;
    match guard.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Ok(0),
        Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
    }
    journal::write(
        &root.join("worker.json"),
        &json!({"pid":std::process::id(),"started":ferry_core::now(),"controller_version":ferry_core::VERSION}),
    )?;
    let next = std::sync::atomic::AtomicUsize::new(0);
    let failures = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..op.parallel.min(op.targets.len()) {
            let (next, failures) = (&next, &failures);
            scope.spawn(move || {
                loop {
                    let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if index >= op.targets.len() {
                        break;
                    }
                    if let Err(e) = one(root, op, index, collect_only) {
                        failures
                            .lock()
                            .unwrap()
                            .push(format!("target {index}: {e}"));
                    }
                }
            });
        }
    });
    for failure in failures.into_inner().unwrap() {
        eprintln!("{failure}");
    }
    drop(guard);
    let report = journal::report(root)?;
    journal::write(&root.join("report.json"), &report)?;
    Ok(i32::from(report["state"] != "succeeded"))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn operation(label: &str) -> (Operation, std::path::PathBuf) {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "ferry-check-{label}-{}-{stamp}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let op=Operation{id:"stable".into(),profile_name:"smoke".into(),profile:serde_json::from_value(json!({"platforms":{"all":{"command":"must-not-run","build":"must-not-build","launch":"must-not-launch"}},"ready_log":"ready"})).unwrap(),cwd:root.clone(),project:None,isolated:false,targets:vec![(None,None)],parallel:1,created:0};
        (op, root)
    }
    fn run<'a>(op: &'a Operation, root: &'a Path) -> Run<'a> {
        Run {
            root,
            op,
            index: 0,
            device: None,
            session: "check-test".into(),
            spec: op.profile.platforms["all"].clone(),
            target: journal::target(root, op, 0).unwrap(),
            collect_only: false,
        }
    }
    #[test]
    fn cancellation_before_start_never_contacts_device_or_starts_work() {
        let (op, root) = operation("cancel");
        std::fs::write(root.join("cancel"), "cancel").unwrap();
        let mut run = run(&op, &root);
        run.execute(None).unwrap();
        assert_eq!(run.target.state, "cancelled");
        assert!(run.target.phases.is_empty());
        assert!(!run.target.cleanup_pending);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn collecting_recovery_does_not_reenter_build_or_launch() {
        let (op, root) = operation("collect");
        let mut run = run(&op, &root);
        run.target.outcome = Some("succeeded".into());
        run.target.state = "incomplete".into();
        run.execute(None).unwrap();
        assert_eq!(run.target.state, "succeeded");
        assert!(run.target.phases.is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn expired_phase_is_journaled_before_any_launch() {
        let (op, root) = operation("expired");
        let mut run = run(&op, &root);
        run.target.deadline = Some(0);
        assert_eq!(run.phase("test", "must-not-run").unwrap(), 0);
        assert_eq!(run.phase("test", "must-not-run").unwrap(), 0);
        let saved: Target = journal::read(&journal::target_path(&root, 0)).unwrap();
        assert_eq!(saved.phases.len(), 1);
        assert_eq!(saved.phases[0].request_id, "stable-0-test");
        assert!(saved.phases[0].run_id.is_none());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn known_completion_is_not_changed_to_timeout_after_worker_restart() {
        let (op, root) = operation("deadline");
        let phase = Phase {
            finished_seen: Some(19),
            result: Some(json!({"state":"succeeded"})),
            ready_log: true,
            ready_probe: true,
            ..Phase::default()
        };
        assert_eq!(
            completed_before_deadline(&phase, 20, true, &op.profile),
            Some(true)
        );
        assert_eq!(
            completed_before_deadline(&phase, 18, true, &op.profile),
            None
        );
        let phase = Phase {
            finished_seen: None,
            ..phase
        };
        assert_eq!(
            completed_before_deadline(&phase, 20, true, &op.profile),
            None
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn collect_only_never_creates_an_unstarted_phase() {
        let (op, root) = operation("collect-only");
        let mut run = run(&op, &root);
        run.collect_only = true;
        assert!(run.phase("build", "must-not-run").is_err());
        assert!(run.target.phases.is_empty());
        assert!(!journal::target_path(&root, 0).exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
