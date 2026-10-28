//! Устройство, которое выполняет: команды, задачи, экран, проект, файлы, туннели. Всё — в
//! сессии пользователя, где живёт приложение: окна видны, ввод работает.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ferry_core::mode::Mode;
use ferry_core::{desk, exit_code, info, jobs, screen, sh_quote};
use irpc::WithChannels;
use irpc::channel::mpsc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::sync::watch;

use crate::files;
use crate::proto::*;

/// Сколько ждать хвост вывода после выхода команды. Запущенные ею фоновые процессы держат
/// pipe открытым — их конца не ждём (как старый демон рабочего стола и ssh).
const TAIL: Duration = Duration::from_millis(500);

fn capture_frames(
    permit: tokio::sync::SemaphorePermit<'static>,
    fps: u32,
    mut capture: impl FnMut(u64) -> Reply<screen::live::Frame> + Send + 'static,
) -> tokio::sync::mpsc::Receiver<Reply<screen::live::Frame>> {
    let (frames, receive) = tokio::sync::mpsc::channel(1);
    tokio::task::spawn_blocking(move || {
        // Native capture cannot be cancelled: keep its slot even after the client leaves.
        let _permit = permit;
        let interval = Duration::from_secs_f64(1.0 / f64::from(fps));
        for sequence in 0.. {
            if frames.is_closed() {
                break;
            }
            let started = std::time::Instant::now();
            let frame = capture(sequence);
            let failed = frame.is_err();
            if frames.blocking_send(frame).is_err() || failed {
                break;
            }
            std::thread::sleep(interval.saturating_sub(started.elapsed()));
        }
    });
    receive
}

async fn relay_screen_frames(
    mut receive: tokio::sync::mpsc::Receiver<Reply<screen::live::Frame>>,
    tx: mpsc::Sender<Reply<screen::live::Frame>>,
    idle: Duration,
) {
    loop {
        let frame = tokio::select! {
            biased;
            _ = tx.closed() => return,
            next = tokio::time::timeout(idle, receive.recv()) => match next {
                Ok(Some(frame)) => frame,
                Ok(None) => return,
                Err(_) => Err(Fail::new("нативный захват не вернул кадр за 20 секунд; поток закрыт, незавершённый вызов ОС продолжает занимать слот")),
            },
        };
        let failed = frame.is_err();
        if !matches!(
            tokio::time::timeout(Duration::from_secs(5), tx.send(frame)).await,
            Ok(Ok(()))
        ) || failed
        {
            return;
        }
    }
}

pub(crate) struct Node {
    /// `~/.ferry`: временные файлы, снимки.
    pub dir: PathBuf,
    /// Дом пользователя: проекты — в `<home>/ferry/<имя>`.
    pub home: PathBuf,
    /// Бинарник с `job-supervise <имя>` — надсмотрщик фоновых задач.
    pub supervisor: PathBuf,
    pub me: watch::Receiver<Status>,
    /// Работающие команды (pid) — для `Stop`.
    pub runs: Mutex<HashSet<u32>>,
    /// Вход релея git: команды, которые прислало устройство, ходят в git его ключами.
    pub relay: Arc<crate::relay::Local>,
    pub update: Option<crate::agent::UpdateHook>,
    pub update_file: Option<crate::agent::LocalUpdateHook>,
    pub update_status: ferry_core::update::Monitor,
}

impl Node {
    /// Можно ли выполнять: роль и режим этого устройства. `desk` — команде нужен рабочий стол.
    fn check(&self, desk: bool) -> Reply<()> {
        let me = self.me.borrow().clone();
        if me.mode == Mode::Paused {
            return Err(Fail::refused(format!(
                "{} отключён от других устройств — это выбрали на самом устройстве (иконка ferry в трее)",
                me.name
            )));
        }
        if !me.role.accepts_commands() {
            return Err(Fail::refused(format!(
                "{} — пульт: команды не принимает (роль меняется в ferry: узел или узел и пульт)",
                me.name
            )));
        }
        if desk && !me.mode.allows_desktop() {
            return Err(Fail::refused(format!(
                "на {} режим «не трогать рабочий стол»: окна, ввод, снимки и задачи отклоняются",
                me.name
            )));
        }
        Ok(())
    }

    fn resolve(&self, dir: &Dir) -> Reply<PathBuf> {
        match dir {
            Dir::Home => Ok(self.home.clone()),
            Dir::Project { name, prefix } => {
                let root = self.home.join("ferry").join(name);
                if !root.join(".git").exists() {
                    let me = self.me.borrow().name.clone();
                    return Err(Fail::new(format!(
                        "на {me} нет проекта {name} — сначала ferry sync (или run без --no-sync); \
                         в доме устройства — ferry run {me} --home -- …"
                    )));
                }
                Ok(root.join(prefix))
            }
        }
    }

    /// Путь для `Get`: `~/…` — от дома, абсолютный — как есть, иначе — от `dir`.
    fn path(&self, path: &str, dir: &Dir) -> Reply<PathBuf> {
        if let Some(rest) = path.strip_prefix("~/") {
            return Ok(self.home.join(rest));
        }
        if Path::new(path).is_absolute() {
            return Ok(path.into());
        }
        Ok(self.resolve(dir)?.join(path))
    }

    /// Шелл устройства (Git Bash на Windows, login-шелл на unix): stdout или ошибка со stderr.
    async fn shell(&self, script: &str, cwd: &Path) -> Reply<Vec<u8>> {
        let mut command = tokio::process::Command::from(desk::shell(script));
        let out = command
            .current_dir(cwd)
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(|e| Fail::new(format!("шелл не запустился: {e}")))?;
        if out.status.success() {
            Ok(out.stdout)
        } else {
            let said = String::from_utf8_lossy(&out.stderr);
            Err(Fail::new(said.trim()))
        }
    }

    /// Запрос связанного устройства `from`.
    pub async fn handle(self: Arc<Self>, msg: DeviceMessage, from: iroh::EndpointId) {
        let (msg, shell) = match msg {
            DeviceMessage::RunShell(WithChannels { inner, tx, .. }) => {
                (DeviceMessage::Run((inner.run, tx).into()), inner.shell)
            }
            DeviceMessage::StartShell(WithChannels { inner, tx, .. }) => {
                (DeviceMessage::Start((inner.start, tx).into()), inner.shell)
            }
            msg => (msg, Shell::Default),
        };
        match msg {
            DeviceMessage::RunShell(_) | DeviceMessage::StartShell(_) => unreachable!(),
            DeviceMessage::Run(WithChannels { inner, tx, .. }) => {
                self.run(inner, from, tx, shell).await
            }
            DeviceMessage::Info(WithChannels { tx, .. }) => {
                let mode = self.me.borrow().mode;
                let result = match mode {
                    Mode::Paused => self.check(false).map(|_| Vec::new()),
                    _ => blocking(info::collect).await,
                };
                tx.send(result).await.ok();
            }
            DeviceMessage::JobResult(WithChannels { inner, tx, .. }) => {
                tx.send(self.job_result(inner, true).await).await.ok();
            }
            DeviceMessage::Storage(WithChannels { inner, tx, .. }) => {
                tx.send(self.storage(inner.0, true).await).await.ok();
            }
            DeviceMessage::Human(WithChannels { inner, tx, .. }) => {
                let owner = format!("{from}:{}", inner.session);
                tx.send(
                    self.human(inner.request, owner, from.fmt_short().to_string(), true)
                        .await,
                )
                .await
                .ok();
            }
            DeviceMessage::AtomicStart(WithChannels { inner, tx, .. }) => {
                tx.send(
                    self.atomic_start(inner.request, None, Some((from, inner.trust)))
                        .await,
                )
                .await
                .ok();
            }
            DeviceMessage::ExecutionCancel(WithChannels { inner, tx, .. }) => {
                tx.send(
                    self.execution_cancel(inner.request_id, inner.session, true)
                        .await,
                )
                .await
                .ok();
            }
            DeviceMessage::ExecutionCancelOwned(WithChannels { inner, tx, .. }) => {
                tx.send(
                    self.execution_cancel_owned(inner.request_id, inner.session, true)
                        .await,
                )
                .await
                .ok();
            }
            DeviceMessage::ScreenStream(WithChannels { inner, tx, .. }) => {
                self.screen_stream(inner.0, tx).await;
            }
            DeviceMessage::ExecutionLookup(WithChannels { inner, tx, .. }) => {
                let result = match self.check(false) {
                    Ok(()) => self.execution_lookup(inner.0).await,
                    Err(e) => Err(e),
                };
                tx.send(result).await.ok();
            }
            DeviceMessage::Workspace(WithChannels { inner, tx, .. }) => {
                let result = match self.check(false) {
                    Ok(()) => self.workspace(inner.0).await,
                    Err(e) => Err(e),
                };
                tx.send(result).await.ok();
            }
            DeviceMessage::ActV2(WithChannels { inner, tx, .. }) => {
                tx.send(self.act_v2(*inner.0, true).await).await.ok();
            }
            DeviceMessage::ObserveV2(WithChannels { inner, tx, .. }) => {
                tx.send(self.observe_v2(inner.0, true).await).await.ok();
            }
            DeviceMessage::Act(WithChannels { inner, tx, .. }) => {
                tx.send(self.act(inner.0, true).await).await.ok();
            }
            DeviceMessage::Observe(WithChannels { inner, tx, .. }) => {
                tx.send(self.observe(inner.0, true).await).await.ok();
            }
            DeviceMessage::Evidence(WithChannels { inner, tx, .. }) => {
                tx.send(self.evidence(inner.0, true).await).await.ok();
            }
            DeviceMessage::Capabilities(WithChannels { inner, tx, .. }) => {
                let result = match self.check(false) {
                    Ok(()) => blocking(move || ferry_core::capabilities::collect(inner.0)).await,
                    Err(e) => Err(e),
                };
                tx.send(result).await.ok();
            }
            DeviceMessage::Jobs(WithChannels { tx, .. }) => {
                let result = match self.check(false) {
                    Ok(()) => blocking(jobs::rows).await,
                    Err(fail) => Err(fail),
                };
                tx.send(result).await.ok();
            }
            DeviceMessage::Start(WithChannels { inner, tx, .. }) => {
                let result = async {
                    self.check(true)?;
                    let cwd = self.resolve(&inner.dir)?;
                    let source = self.supervisor.clone();
                    let state = self.dir.clone();
                    let supervisor =
                        blocking(move || jobs::prepare_supervisor(&state, &source)).await?;
                    let env = self
                        .relay
                        .env(from, &inner.trust, &supervisor)
                        .map_err(Fail::new)?;
                    blocking(move || {
                        jobs::start_in_shell(
                            &inner.name,
                            &inner.cmd,
                            &cwd,
                            &env,
                            &supervisor,
                            shell,
                        )
                    })
                    .await
                };
                tx.send(result.await).await.ok();
            }
            DeviceMessage::Log(WithChannels { inner, tx, .. }) => self.log(inner, tx).await,
            DeviceMessage::Kill(WithChannels { inner, tx, .. }) => {
                let result = match self.check(false) {
                    Ok(()) => blocking(move || jobs::stop_job(&inner.name)).await,
                    Err(fail) => Err(fail),
                };
                tx.send(result).await.ok();
            }
            DeviceMessage::Shot(WithChannels { tx, .. }) => {
                let result = async {
                    self.check(true)?;
                    let file = files::temp(&self.dir, "png")?;
                    let path = file.clone();
                    let code =
                        blocking(move || screen::screenshot(&path.to_string_lossy())).await?;
                    if code != 0 || !file.exists() {
                        return Err(Fail::new(format!("снимок экрана не вышел (код {code})")));
                    }
                    Ok(file)
                };
                files::reply_file(result.await, &tx).await;
            }
            DeviceMessage::ShotWindow(WithChannels { inner, tx, .. }) => {
                let result = async {
                    self.check(true)?;
                    let file = files::temp(&self.dir, "png")?;
                    let path = file.clone();
                    let code = blocking(move || {
                        screen::screenshot_window(&path.to_string_lossy(), &inner.name)
                    })
                    .await?;
                    if code != 0 || !file.exists() {
                        return Err(Fail::new(format!("снимок окна не вышел (код {code})")));
                    }
                    Ok(file)
                };
                files::reply_file(result.await, &tx).await;
            }
            DeviceMessage::ShotArea(WithChannels { inner, tx, .. }) => {
                let result = async {
                    self.check(true)?;
                    let file = files::temp(&self.dir, "png")?;
                    let path = file.clone();
                    let code = blocking(move || {
                        screen::screenshot_area(&path.to_string_lossy(), inner.area)
                    })
                    .await?;
                    if code != 0 || !file.exists() {
                        return Err(Fail::new(format!("снимок экрана не вышел (код {code})")));
                    }
                    Ok(file)
                };
                files::reply_file(result.await, &tx).await;
            }
            DeviceMessage::Input(WithChannels { inner, tx, .. }) => {
                let result = match self.check(true) {
                    Ok(()) => {
                        blocking(move || {
                            let args: Vec<&str> = inner.args.iter().map(String::as_str).collect();
                            screen::input(&args)
                        })
                        .await
                    }
                    Err(fail) => Err(fail),
                };
                tx.send(result).await.ok();
            }
            DeviceMessage::Rec(WithChannels { inner, tx, .. }) => {
                let result = async {
                    self.check(true)?;
                    let frames = files::temp(&self.dir, "rec")?;
                    let (path, Rec { seconds, fps }) = (frames.clone(), inner);
                    let code =
                        blocking(move || screen::record(&seconds, &fps, &path.to_string_lossy()))
                            .await?;
                    if code != 0 {
                        return Err(Fail::new(format!("запись экрана не вышла (код {code})")));
                    }
                    let archive = files::temp(&self.dir, "tar")?;
                    let (from, to) = (frames.clone(), archive.clone());
                    blocking(move || {
                        let rels: Vec<PathBuf> = std::fs::read_dir(&from)?
                            .flatten()
                            .map(|e| PathBuf::from(e.file_name()))
                            .collect();
                        files::pack(&from, &rels, &to)
                    })
                    .await?;
                    let _ = std::fs::remove_dir_all(&frames);
                    Ok(archive)
                };
                files::reply_file(result.await, &tx).await;
            }
            DeviceMessage::Power(WithChannels { inner, tx, .. }) => {
                let result = async {
                    self.check(false)?;
                    if !cfg!(windows) {
                        return Err(Fail::new("питание пока умею только на Windows"));
                    }
                    let (script, done) = ferry_core::power::command(&inner.action)?;
                    self.shell(&script, &self.home).await?;
                    Ok(format!("ferry: {} {done}", self.me.borrow().name))
                };
                tx.send(result.await).await.ok();
            }
            DeviceMessage::Stop(WithChannels { tx, .. }) => {
                let result = async {
                    self.check(false)?;
                    let runs: Vec<u32> = self.runs.lock().unwrap().drain().collect();
                    for pid in &runs {
                        let _ = desk::kill_tree(*pid);
                    }
                    let killed = blocking(jobs::kill_all).await?;
                    Ok(format!(
                        "ferry: остановлено команд: {}, задач: {killed}",
                        runs.len()
                    ))
                };
                tx.send(result.await).await.ok();
            }
            DeviceMessage::SyncPrepare(WithChannels { inner, tx, .. }) => {
                let result = async {
                    self.check(false)?;
                    let script = ferry_core::sync::changes_script(&inner.project, &inner.keep);
                    let changes = self.shell(&script, &self.home).await?;
                    let repo = self.home.join("ferry").join(&inner.project);
                    let head = self
                        .shell("git rev-parse -q --verify HEAD", &repo)
                        .await
                        .ok();
                    let head = head.map(|h| String::from_utf8_lossy(&h).trim().to_string());
                    Ok(Prepared {
                        head: head.filter(|h| !h.is_empty()),
                        changes,
                    })
                };
                tx.send(result.await).await.ok();
            }
            DeviceMessage::SyncApply(WithChannels {
                inner, tx, mut rx, ..
            }) => {
                let result = async {
                    self.check(false)?;
                    let repo = self.home.join("ferry").join(&inner.project);
                    files::recv_file(&mut rx, &repo.join(".git/ferry.bundle")).await?;
                    // Пустой бандл — снимок уже здесь, нужен только checkout поверх правок.
                    let script = format!(
                        "{{ [ ! -s .git/ferry.bundle ] || git fetch -q -f .git/ferry.bundle \
                         refs/ferry/snapshot:refs/ferry/snapshot; }} \
                         && git checkout -q -f --detach {}; rc=$?; rm -f .git/ferry.bundle; exit $rc",
                        sh_quote(&inner.commit)
                    );
                    self.shell(&script, &repo).await.map(drop)
                };
                tx.send(result.await).await.ok();
            }
            DeviceMessage::Put(WithChannels {
                inner, tx, mut rx, ..
            }) => {
                let result = async {
                    self.check(false)?;
                    let dest = self.resolve(&inner.dir)?;
                    let archive = files::temp(&self.dir, "tar")?;
                    files::recv_file(&mut rx, &archive).await?;
                    let from = archive.clone();
                    let unpacked = blocking(move || files::unpack(&from, &dest)).await;
                    let _ = std::fs::remove_file(&archive);
                    unpacked.map(drop)
                };
                tx.send(result.await).await.ok();
            }
            DeviceMessage::Get(WithChannels { inner, tx, .. }) => {
                let result = async {
                    self.check(false)?;
                    let path = self.path(&inner.path, &inner.dir)?;
                    let name = path
                        .file_name()
                        .ok_or_else(|| {
                            Fail::new(format!("{}: назови файл или каталог", inner.path))
                        })?
                        .into();
                    if !path.exists() {
                        return Err(Fail::new(format!("{}: нет такого файла", inner.path)));
                    }
                    let base = path.parent().unwrap_or(&path).to_path_buf();
                    let archive = files::temp(&self.dir, "tar")?;
                    let to = archive.clone();
                    blocking(move || files::pack(&base, &[name], &to)).await?;
                    Ok(archive)
                };
                files::reply_file(result.await, &tx).await;
            }
            DeviceMessage::Tunnel(WithChannels { inner, tx, rx, .. }) => {
                if self.check(false).is_ok() {
                    tunnel(inner.port, tx, rx).await;
                }
            }
            // Статус, роль и имя — у агента.
            DeviceMessage::GitSsh(WithChannels { inner, tx, rx, .. }) => {
                let me = self.me.borrow().clone();
                crate::relay::serve_git_ssh(&self.dir, &me, inner, tx, rx).await;
            }
            DeviceMessage::UpdateStatus(WithChannels { tx, .. }) => {
                let status = self.update_status.lock().unwrap().clone();
                tx.send(Ok(status)).await.ok();
            }
            DeviceMessage::UpdateFile(WithChannels {
                inner, tx, mut rx, ..
            }) => {
                let result = async {
                    self.check_update_file(&inner.signature)?;
                    let file = files::temp(&self.dir, "update")?;
                    let received =
                        files::recv_file_limited(&mut rx, &file, ferry_core::update::MAX_PACKAGE)
                            .await;
                    let result = match received {
                        Ok(()) => self.install_file(file.clone(), inner.signature).await,
                        Err(fail) => Err(fail),
                    };
                    let _ = std::fs::remove_file(file);
                    result
                }
                .await;
                tx.send(result).await.ok();
            }
            DeviceMessage::Update(WithChannels { tx, .. }) => {
                tx.send(self.update().await).await.ok();
            }
            DeviceMessage::Logs(WithChannels {
                tx,
                inner: Logs { lines },
                ..
            }) => {
                tx.send(self.logs(lines)).await.ok();
            }
            DeviceMessage::Watch(_)
            | DeviceMessage::SetRole(_)
            | DeviceMessage::Rename(_)
            | DeviceMessage::Desk(_) => {}
            DeviceMessage::Peers(WithChannels { tx, .. }) => {
                tx.send(()).await.ok();
            }
            DeviceMessage::Crashes(WithChannels { inner, tx, .. }) => {
                let result = match self.check(false) {
                    Ok(()) => {
                        blocking(move || ferry_core::crashes::report(&inner.process, inner.last))
                            .await
                    }
                    Err(fail) => Err(fail),
                };
                tx.send(result).await.ok();
            }
            DeviceMessage::PortOpen(WithChannels { inner, tx, .. }) => {
                let result = match self.check(false) {
                    Ok(()) => Ok(knock(inner.port).await),
                    Err(fail) => Err(fail),
                };
                tx.send(result).await.ok();
            }
        }
    }

    /// Последние `lines` строк лога приложения. Отключённое устройство не отвечает и на это.
    pub fn logs(&self, lines: usize) -> Reply<String> {
        let me = self.me.borrow().clone();
        if me.mode == Mode::Paused {
            return Err(Fail::refused(format!(
                "{} отключён от других устройств",
                me.name
            )));
        }
        let path = ferry_core::app_log(&self.dir);
        let text = std::fs::read(&path)
            .map_err(|e| Fail::new(format!("{}: лог {}: {e}", me.name, path.display())))?;
        let text = String::from_utf8_lossy(&text);
        let all: Vec<&str> = text.lines().collect();
        Ok(all[all.len().saturating_sub(lines)..].join("\n"))
    }

    /// Обновить ferry из релиза. Отключённое человеком устройство издалека не трогаем.
    pub async fn update(&self) -> Reply<String> {
        let me = self.me.borrow().clone();
        if me.mode == Mode::Paused {
            return Err(Fail::refused(format!(
                "{} отключён от других устройств",
                me.name
            )));
        }
        let Some(update) = &self.update else {
            return Err(Fail::new(format!(
                "{}: эта сборка ferry не обновляется сама",
                me.name
            )));
        };
        update().await.map_err(Fail::new)
    }

    fn check_update_file(&self, signature: &str) -> Reply<()> {
        if self.me.borrow().mode != Mode::Active {
            return Err(Fail::refused(
                "для установки локальной сборки включи активный режим на устройстве",
            ));
        }
        if self.update_file.is_none() {
            return Err(Fail::new("эта сборка ferry не обновляется сама"));
        }
        let status = self.update_status.lock().unwrap();
        if status.busy {
            return Err(Fail::new("обновление уже выполняется"));
        }
        ferry_core::update::Package::parse(signature)?.check_target(&status)
    }
    pub async fn install_file(&self, file: PathBuf, signature: String) -> Reply<String> {
        self.check_update_file(&signature)?;
        ferry_core::update::check_size(&file)?;
        self.update_file.as_ref().unwrap()(file, signature)
            .await
            .map_err(Fail::new)
    }

    async fn run(
        &self,
        Run {
            cmd,
            dir,
            desk,
            trust,
        }: Run,
        from: iroh::EndpointId,
        tx: mpsc::Sender<Output>,
        shell: Shell,
    ) {
        let spawned = (|| {
            self.check(desk)?;
            let cwd = self.resolve(&dir)?;
            tokio::process::Command::from(
                desk::shell_with(&cmd, shell).map_err(|e| Fail::new(e.to_string()))?,
            )
            .envs(
                self.relay
                    .env(from, &trust, &self.supervisor)
                    .map_err(Fail::new)?,
            )
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Fail::new(format!("шелл не запустился: {e}")))
        })();
        let mut child = match spawned {
            Ok(child) => child,
            Err(fail) => {
                tx.send(Output::Fail(fail)).await.ok();
                return;
            }
        };
        let pid = child.id().unwrap_or_default();
        let _tree = desk::Tree::track(pid);
        self.runs.lock().unwrap().insert(pid);
        let (events_tx, mut events) = tokio::sync::mpsc::channel(16);
        pump(child.stdout.take().unwrap(), events_tx.clone(), Output::Out);
        pump(child.stderr.take().unwrap(), events_tx, Output::Err);
        let code = loop {
            tokio::select! {
                Some(event) = events.recv() => {
                    if tx.send(event).await.is_err() {
                        break None;
                    }
                }
                status = child.wait() => break status.ok().map(exit_code),
                // Вызвавший ушёл (Ctrl-C, обрыв связи) — команда ему больше не нужна.
                _ = tx.closed() => break None,
            }
        };
        match code {
            Some(code) => {
                let tail = tokio::time::sleep(TAIL);
                tokio::pin!(tail);
                loop {
                    tokio::select! {
                        event = events.recv() => match event {
                            Some(event) => { tx.send(event).await.ok(); }
                            None => break,
                        },
                        _ = &mut tail => break,
                    }
                }
                tx.send(Output::Exit(code)).await.ok();
            }
            None => {
                let _ = desk::kill_tree(pid);
            }
        }
        self.runs.lock().unwrap().remove(&pid);
    }

    pub async fn screen_stream(
        self: &Arc<Self>,
        request: screen::live::Request,
        tx: mpsc::Sender<Reply<screen::live::Frame>>,
    ) {
        static CAPTURES: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
        let Ok(permit) = CAPTURES.try_acquire() else {
            let _ = tokio::time::timeout(Duration::from_secs(5), tx.send(Err(Fail::new(
                "оба слота захвата заняты: активным потоком или незавершённым вызовом ОС; зависший вызов удерживает слот до возврата или ручного перезапуска ferry",
            )))).await;
            return;
        };
        if let Err(e) = request.validate() {
            let _ = tokio::time::timeout(Duration::from_secs(5), tx.send(Err(e.into()))).await;
            return;
        }
        let node = self.clone();
        let receive = capture_frames(permit, request.fps, move |sequence| {
            node.check(true).and_then(|()| {
                screen::live::frame(&node.dir, &request, sequence).map_err(Fail::from)
            })
        });
        relay_screen_frames(receive, tx, Duration::from_secs(20)).await;
    }

    pub async fn storage(
        &self,
        request: ferry_core::storage::Request,
        remote: bool,
    ) -> Reply<String> {
        if remote {
            self.check(false)?;
        }
        let root = self.dir.clone();
        let home = self.home.clone();
        blocking(move || Ok(ferry_core::storage::execute(&root, &home, request)?.to_string())).await
    }
    pub async fn human(
        &self,
        request: ferry_core::human::Request,
        owner: String,
        label: String,
        remote: bool,
    ) -> Reply<String> {
        if remote {
            self.check(matches!(request, ferry_core::human::Request::Ask { .. }))?;
        }
        let root = self.dir.clone();
        blocking(
            move || Ok(ferry_core::human::execute(&root, &owner, &label, request)?.to_string()),
        )
        .await
    }
    pub async fn atomic_start(
        &self,
        request: jobs::execution::StartRequest,
        local_cwd: Option<PathBuf>,
        remote: Option<(iroh::EndpointId, Vec<String>)>,
    ) -> Reply<jobs::execution::Started> {
        let desktop = request.resource == jobs::execution::Resource::Desktop;
        self.check(desktop)?;
        if remote.is_some() && local_cwd.is_some() {
            return Err(Fail::new("remote start не принимает local_cwd"));
        }
        let cwd = match local_cwd {
            Some(path) => path,
            None => self.resolve(&request.dir)?,
        };
        let state = self.dir.clone();
        let source = self.supervisor.clone();
        let supervisor = blocking(move || jobs::prepare_supervisor(&state, &source)).await?;
        let env = match remote {
            Some((from, trust)) => self
                .relay
                .env(from, &trust, &supervisor)
                .map_err(Fail::new)?,
            None => Vec::new(),
        };
        let root = self.dir.clone();
        tokio::task::spawn_blocking(move || {
            jobs::execution::start_at(&root, request, &cwd, &env, &supervisor)
        })
        .await
        .map_err(Fail::new)?
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::WouldBlock {
                Fail::refused(e)
            } else {
                Fail::new(e)
            }
        })
    }
    pub async fn execution_cancel(
        &self,
        id: String,
        session: String,
        remote: bool,
    ) -> Reply<String> {
        if remote {
            self.check(false)?;
        }
        let root = self.dir.clone();
        blocking(move || Ok(jobs::execution::cancel(&root, &id, &session)?.to_string())).await
    }
    pub async fn execution_cancel_owned(
        &self,
        id: String,
        session: String,
        remote: bool,
    ) -> Reply<String> {
        if remote {
            self.check(false)?;
        }
        let root = self.dir.clone();
        blocking(move || Ok(jobs::execution::cancel_owned(&root, &id, &session)?.to_string())).await
    }
    pub async fn execution_lookup(&self, id: String) -> Reply<Option<jobs::execution::Started>> {
        let root = self.dir.clone();
        blocking(move || jobs::execution::lookup(&root, &id)).await
    }
    pub async fn workspace(&self, request: ferry_core::workspace::Request) -> Reply<String> {
        let home = self.home.clone();
        blocking(move || Ok(ferry_core::workspace::execute(&home, request)?.to_string())).await
    }
    pub async fn act_v2(
        &self,
        request: screen::act::RequestV2,
        remote: bool,
    ) -> Reply<screen::act::ReportV2> {
        if remote {
            self.check(true)?;
        }
        let root = self.dir.clone();
        blocking(move || screen::act::execute_v2(request, &root)).await
    }
    pub async fn observe_v2(
        &self,
        request: screen::observe::RequestV2,
        remote: bool,
    ) -> Reply<screen::observe::ObservationV2> {
        if remote {
            self.check(true)?;
        }
        let root = self.dir.clone();
        blocking(move || screen::observe::capture_v2(request, &root)).await
    }

    pub async fn act(
        &self,
        request: ferry_core::screen::act::Request,
        remote: bool,
    ) -> Reply<ferry_core::screen::act::Report> {
        if remote {
            self.check(true)?;
        }
        let root = self.dir.clone();
        blocking(move || ferry_core::screen::act::execute(request, &root)).await
    }
    pub async fn observe(
        &self,
        request: ferry_core::screen::observe::Request,
        remote: bool,
    ) -> Reply<ferry_core::screen::observe::Observation> {
        if remote {
            self.check(true)?;
        }
        let root = self.dir.clone();
        blocking(move || ferry_core::screen::observe::capture(request, &root)).await
    }

    pub async fn evidence(
        &self,
        query: ferry_core::jobs::evidence::Query,
        remote: bool,
    ) -> Reply<String> {
        if remote {
            self.check(matches!(
                &query.action,
                jobs::evidence::Action::Probe {
                    window: Some(_),
                    ..
                }
            ))?;
        }
        let root = self.dir.clone();
        blocking(move || {
            Ok(serde_json::to_string(&jobs::evidence::execute(
                &root, query,
            )?)?)
        })
        .await
    }

    pub async fn job_result(&self, request: JobResult, remote: bool) -> Reply<jobs::JobResult> {
        if request.wait > 60 {
            return Err(Fail::new("--wait: от 0 до 60 секунд"));
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(request.wait.into());
        let mut generation = None;
        loop {
            if remote {
                self.check(false)?;
            }
            let (root, name, cursor, limit) = (
                self.dir.clone(),
                request.name.clone(),
                request.cursor.clone(),
                request.max_bytes,
            );
            let result =
                blocking(move || jobs::result_in(&root, &name, cursor.as_deref(), limit)).await?;
            if generation.as_ref().is_some_and(|id| id != &result.run_id) {
                return Err(Fail::new(
                    "задача перезапущена во время ожидания; запроси result заново",
                ));
            }
            generation = Some(result.run_id.clone());
            if result.state != "running"
                || tokio::time::Instant::now() >= deadline
                || (request.cursor.is_some() && !result.log.is_empty())
            {
                return Ok(result);
            }
            tokio::time::sleep_until(
                deadline.min(tokio::time::Instant::now() + Duration::from_millis(250)),
            )
            .await;
        }
    }

    async fn log(&self, Log { name, follow }: Log, tx: mpsc::Sender<Output>) {
        let file = match self.check(false).and_then(|()| Ok(jobs::log_file(&name)?)) {
            Ok(file) => file,
            Err(fail) => {
                tx.send(Output::Fail(fail)).await.ok();
                return;
            }
        };
        let Ok(mut file) = tokio::fs::File::open(file).await else {
            tx.send(Output::Fail(Fail::new(format!(
                "лог задачи {name} не открылся"
            ))))
            .await
            .ok();
            return;
        };
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            // Работает ли — до чтения: строки, дописанные перед выходом, тоже попадут.
            let Ok((alive, exit, state)) = jobs::progress(&name) else {
                return;
            };
            loop {
                match file.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send(Output::Out(buf[..n].to_vec())).await.is_err() {
                            return;
                        }
                    }
                }
            }
            if !follow {
                tx.send(Output::Exit(0)).await.ok();
                return;
            }
            if !alive {
                let said = format!("ferry: задача {name}: {state}\n");
                tx.send(Output::Err(said.into_bytes())).await.ok();
                tx.send(Output::Exit(exit.unwrap_or(1))).await.ok();
                return;
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(300)) => {}
                _ = tx.closed() => return,
            }
        }
    }
}

/// Блокирующее (PowerShell, файлы, архивы) — не в рабочих потоках tokio.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> std::io::Result<T> + Send + 'static,
) -> Reply<T> {
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result.map_err(Fail::from),
        Err(e) => Err(Fail::new(format!("упало: {e}"))),
    }
}

/// Читать поток процесса кусками и отдавать событиями.
fn pump(
    mut reader: impl AsyncRead + Unpin + Send + 'static,
    events: tokio::sync::mpsc::Sender<Output>,
    wrap: fn(Vec<u8>) -> Output,
) {
    tokio::spawn(async move {
        let mut buf = vec![0u8; 16 * 1024];
        while let Ok(n) = reader.read(&mut buf).await {
            if n == 0 || events.send(wrap(buf[..n].to_vec())).await.is_err() {
                break;
            }
        }
    });
}

/// TCP-соединение на `port` этого устройства ↔ поток irpc.
async fn tunnel(port: u16, tx: mpsc::Sender<Vec<u8>>, rx: mpsc::Receiver<Vec<u8>>) {
    if let Some(stream) = local(port).await {
        pipe(stream, tx, rx).await;
    }
}

/// Соединение на `port` этого устройства — на `127.0.0.1` или `[::1]`, кто ответит первым:
/// dev-серверы на `localhost` слушают то один адрес, то другой (Vite на маке — только `::1`).
/// Наперегонки, а не по очереди: отказ на закрытом порту Windows отдаёт только через ~2 с.
async fn local(port: u16) -> Option<tokio::net::TcpStream> {
    let connect = |ip: std::net::IpAddr| async move {
        let connect = tokio::net::TcpStream::connect((ip, port));
        match tokio::time::timeout(Duration::from_secs(2), connect).await {
            Ok(Ok(stream)) => Some(stream),
            _ => None,
        }
    };
    let v4 = connect(std::net::Ipv4Addr::LOCALHOST.into());
    let v6 = connect(std::net::Ipv6Addr::LOCALHOST.into());
    tokio::pin!(v4, v6);
    tokio::select! {
        Some(stream) = &mut v4 => Some(stream),
        Some(stream) = &mut v6 => Some(stream),
        // Оба не ответили — select попадает сюда, когда обе ветки вернули None.
        else => None,
    }
}

/// Слушает ли кто-нибудь `port`. Закрываем вежливо: дочитываем, что сервер успел прислать, —
/// закрытие с непрочитанными данными шлёт RST, и сервер без обработчика ошибок падает.
async fn knock(port: u16) -> bool {
    let Some(mut stream) = local(port).await else {
        return false;
    };
    let _ = stream.shutdown().await;
    let mut buf = [0u8; 4096];
    let drain = async {
        while let Ok(n) = stream.read(&mut buf).await {
            if n == 0 {
                break;
            }
        }
    };
    let _ = tokio::time::timeout(Duration::from_millis(200), drain).await;
    true
}

/// Байты TCP-соединения ↔ поток irpc, пока одна из сторон не закроется.
pub(crate) async fn pipe(
    stream: tokio::net::TcpStream,
    tx: mpsc::Sender<Vec<u8>>,
    mut rx: mpsc::Receiver<Vec<u8>>,
) {
    let (mut read, mut write) = stream.into_split();
    let up = async {
        while let Ok(Some(data)) = rx.recv().await {
            if write.write_all(&data).await.is_err() {
                break;
            }
        }
        let _ = write.shutdown().await;
    };
    let down = async {
        let mut buf = vec![0u8; 64 * 1024];
        while let Ok(n) = read.read(&mut buf).await {
            if n == 0 || tx.send(buf[..n].to_vec()).await.is_err() {
                break;
            }
        }
    };
    tokio::join!(up, down);
}

#[cfg(test)]
mod screen_stream_tests {
    use super::*;

    #[tokio::test]
    async fn idle_capture_reports_failure_and_drops_frame_receiver() {
        let (frames, receive) = tokio::sync::mpsc::channel(1);
        let (tx, mut rx) = mpsc::channel(1);
        tokio::time::timeout(
            Duration::from_secs(1),
            relay_screen_frames(receive, tx, Duration::from_millis(20)),
        )
        .await
        .unwrap();
        let error = rx.recv().await.unwrap().unwrap().unwrap_err();
        assert!(error.to_string().contains("нативный захват"));
        assert!(rx.recv().await.unwrap().is_none());
        assert!(frames.is_closed());
    }

    #[tokio::test]
    async fn disconnect_is_prompt_but_hung_native_call_keeps_its_slot() {
        static SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);
        let (started, ready) = tokio::sync::oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let mut started = Some(started);
        let receive = capture_frames(SLOTS.try_acquire().unwrap(), 10, move |_| {
            started.take().unwrap().send(()).unwrap();
            let _ = blocked.recv_timeout(Duration::from_secs(2));
            Err(Fail::new("fixture finished"))
        });
        ready.await.unwrap();
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        tokio::time::timeout(
            Duration::from_millis(200),
            relay_screen_frames(receive, tx, Duration::from_secs(20)),
        )
        .await
        .unwrap();
        assert!(
            SLOTS.try_acquire().is_err(),
            "a disconnected native call must retain its slot"
        );
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), SLOTS.acquire())
            .await
            .unwrap()
            .unwrap();
    }
}
