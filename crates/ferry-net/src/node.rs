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
                        "на {me} нет проекта {name} — сначала ferry sync"
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
        match msg {
            DeviceMessage::Run(WithChannels { inner, tx, .. }) => self.run(inner, from, tx).await,
            DeviceMessage::Info(WithChannels { tx, .. }) => {
                let mode = self.me.borrow().mode;
                let result = match mode {
                    Mode::Paused => self.check(false).map(|_| Vec::new()),
                    _ => blocking(info::collect).await,
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
                    let supervisor = self.supervisor.clone();
                    let env = self.relay.env(from, &inner.trust, &supervisor);
                    blocking(move || {
                        jobs::start_in(&inner.name, &inner.cmd, &cwd, &env, &supervisor)
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
            DeviceMessage::Watch(_) | DeviceMessage::SetRole(_) | DeviceMessage::Rename(_) => {}
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
    ) {
        let spawned = (|| {
            self.check(desk)?;
            let cwd = self.resolve(&dir)?;
            tokio::process::Command::from(desk::shell(&cmd))
                .envs(self.relay.env(from, &trust, &self.supervisor))
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

/// TCP-соединение на `127.0.0.1:<port>` этого устройства ↔ поток irpc.
async fn tunnel(port: u16, tx: mpsc::Sender<Vec<u8>>, rx: mpsc::Receiver<Vec<u8>>) {
    if let Ok(stream) = tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
        pipe(stream, tx, rx).await;
    }
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
