//! Команды связанному устройству — сторона того, кто вызывает: CLI и приложение работают через
//! [`Device`]. Само устройство выполняет их у себя (`node.rs`).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use ferry_core::jobs::Job;
use iroh::EndpointId;
use irpc::channel::mpsc;

use crate::files;
use crate::proto::*;

/// Связанное устройство на связи.
#[derive(Clone)]
pub struct Device {
    pub id: EndpointId,
    pub name: String,
    pub(crate) client: DeviceClient,
    /// `~/.ferry` этого устройства: временные файлы, сохранённые патчи.
    pub(crate) dir: PathBuf,
    /// Версия ferry и система устройства (`macos`, `windows`), если статус уже пришёл.
    pub(crate) version: Option<String>,
    pub(crate) os: Option<String>,
}

impl Device {
    /// Команда шеллу устройства: вывод потоком, в конце — код выхода. Бросил приёмник — команда
    /// на устройстве останавливается.
    pub async fn run(&self, cmd: &str, dir: Dir, desk: bool) -> Reply<mpsc::Receiver<Output>> {
        let run = Run {
            cmd: cmd.into(),
            dir,
            desk,
            trust: self.trust(),
        };
        self.client.0.server_streaming(run, 64).await.map_err(lost)
    }

    pub async fn log(&self, name: &str, follow: bool) -> Reply<mpsc::Receiver<Output>> {
        let log = Log {
            name: name.into(),
            follow,
        };
        self.client.0.server_streaming(log, 64).await.map_err(lost)
    }

    pub async fn info(&self) -> Reply<Vec<(String, String)>> {
        self.client.ask(Info).await
    }

    pub async fn jobs(&self) -> Reply<Vec<Job>> {
        self.client.ask(Jobs).await
    }

    pub async fn start(&self, name: &str, cmd: &str, dir: Dir) -> Reply<String> {
        self.client
            .ask(Start {
                name: name.into(),
                cmd: cmd.into(),
                dir,
                trust: self.trust(),
            })
            .await
    }

    pub async fn kill(&self, name: &str) -> Reply<String> {
        self.client.ask(Kill { name: name.into() }).await
    }

    /// Падения программ на устройстве: список или разбор самого свежего.
    pub async fn crashes(&self, process: &str, last: bool) -> Reply<String> {
        need(
            &self.name,
            self.version.as_deref(),
            "0.1.11",
            "ferry crashes",
        )?;
        let process = process.into();
        self.client.ask(Crashes { process, last }).await
    }

    /// Слушает ли кто-нибудь `port` на устройстве.
    pub async fn port_open(&self, port: u16) -> Reply<bool> {
        need(
            &self.name,
            self.version.as_deref(),
            "0.1.11",
            "ferry wait --port",
        )?;
        self.client.ask(PortOpen { port }).await
    }

    pub async fn input(&self, args: Vec<String>) -> Reply<String> {
        if let Some((since, what)) = input_since(&args, self.os.as_deref()) {
            need(&self.name, self.version.as_deref(), since, what)?;
        }
        self.client.ask(Input { args }).await
    }

    pub async fn power(&self, action: &str) -> Reply<String> {
        self.client
            .ask(Power {
                action: action.into(),
            })
            .await
    }

    /// Обновить ferry на устройстве из релиза; оно перезапустится и снова выйдет на связь.
    pub async fn update(&self) -> Reply<String> {
        self.client.ask(Update).await
    }

    /// Хвост лога приложения ferry на устройстве.
    pub async fn logs(&self, lines: usize) -> Reply<String> {
        need(&self.name, self.version.as_deref(), "0.1.3", "ferry logs")?;
        self.client.ask(Logs { lines }).await
    }

    pub async fn stop(&self) -> Reply<String> {
        self.client.ask(Stop).await
    }

    /// Снимок экрана устройства в PNG-файл `file`.
    /// Снимок самого окна — даже закрытого другими.
    pub async fn shot_window(&self, file: &Path, name: &str) -> Reply<()> {
        need(&self.name, self.version.as_deref(), "0.1.7", "снимок окна")?;
        let shot = ShotWindow { name: name.into() };
        let mut rx = self
            .client
            .0
            .server_streaming(shot, 8)
            .await
            .map_err(lost)?;
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        files::recv_file(&mut rx, file).await
    }

    /// Снимок области `[x, y, ширина, высота]` — окна или части экрана.
    pub async fn shot_area(&self, file: &Path, area: [i32; 4]) -> Reply<()> {
        need(
            &self.name,
            self.version.as_deref(),
            "0.1.6",
            "снимок окна и области",
        )?;
        let mut rx = self
            .client
            .0
            .server_streaming(ShotArea { area }, 8)
            .await
            .map_err(lost)?;
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        files::recv_file(&mut rx, file).await
    }

    pub async fn shot(&self, file: &Path) -> Reply<()> {
        let mut rx = self
            .client
            .0
            .server_streaming(Shot, 8)
            .await
            .map_err(lost)?;
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        files::recv_file(&mut rx, file).await
    }

    /// Запись экрана: кадры и лист-обзор — в каталог `dest`.
    pub async fn rec(&self, seconds: &str, fps: &str, dest: &Path) -> Reply<Vec<String>> {
        let rec = Rec {
            seconds: seconds.into(),
            fps: fps.into(),
        };
        let rx = self.client.0.server_streaming(rec, 8).await.map_err(lost)?;
        self.receive(rx, dest).await
    }

    /// Файл или каталог с устройства — в каталог `dest`; ответ — что легло.
    pub async fn get(&self, path: &str, dir: Dir, dest: &Path) -> Reply<Vec<String>> {
        let get = Get {
            path: path.into(),
            dir,
        };
        let rx = self.client.0.server_streaming(get, 8).await.map_err(lost)?;
        self.receive(rx, dest).await
    }

    async fn receive(&self, mut rx: mpsc::Receiver<Chunk>, dest: &Path) -> Reply<Vec<String>> {
        let archive = files::temp(&self.dir, "tar")?;
        let received = files::recv_file(&mut rx, &archive).await;
        let result = match received {
            Ok(()) => {
                let (from, dest) = (archive.clone(), dest.to_path_buf());
                blocking(move || files::unpack(&from, &dest)).await
            }
            Err(fail) => Err(fail),
        };
        let _ = std::fs::remove_file(&archive);
        result
    }

    /// Файлы `rels` (пути относительно `base`) — в `dir` на устройстве.
    pub async fn put(&self, base: &Path, rels: &[PathBuf], dir: Dir) -> Reply<()> {
        let archive = files::temp(&self.dir, "tar")?;
        let (from, rels, to) = (base.to_path_buf(), rels.to_vec(), archive.clone());
        blocking(move || files::pack(&from, &rels, &to)).await?;
        let (tx, reply) = self
            .client
            .0
            .client_streaming(Put { dir }, 8)
            .await
            .map_err(lost)?;
        let sent = files::send_file(&archive, &tx).await;
        let _ = std::fs::remove_file(&archive);
        sent?;
        drop(tx);
        reply.await.map_err(lost)?
    }

    /// Снимок рабочего дерева `commit` (с деревом `tree`) — в `~/ferry/<project>` устройства,
    /// затем файлы мимо git (`files`, пути от `root`). Правки, сделанные на устройстве, перед
    /// перезаписью сохраняются патчем. Ответ — что сделано, для человека.
    pub async fn sync(
        &self,
        root: &Path,
        project: &str,
        commit: &str,
        tree: &str,
        extra: &[PathBuf],
    ) -> Reply<String> {
        let prepared = self
            .client
            .ask(SyncPrepare {
                project: project.into(),
                keep: tree.into(),
            })
            .await?;
        let mut said = Vec::new();
        if !prepared.changes.is_empty() {
            let saved = self.save_patch(project, &prepared.changes)?;
            said.push(format!(
                "ferry: на {} были правки, которых нет здесь; сохранил: {} (вернуть: git apply <патч>)",
                self.name,
                saved.display()
            ));
        }
        if prepared.head.as_deref() != Some(commit) || !prepared.changes.is_empty() {
            // Бандл — только недостающее: всё, чего нет в снимке, который уже лежит на устройстве.
            // Снимок тот же, а дерево на устройстве правили — бандл пустой, только checkout.
            let bundle = files::temp(&self.dir, "bundle")?;
            if prepared.head.as_deref() != Some(commit) {
                let (root, commit, head, path) = (
                    root.to_path_buf(),
                    commit.to_string(),
                    prepared.head.clone(),
                    bundle.clone(),
                );
                blocking(move || make_bundle(&root, &commit, head.as_deref(), &path)).await?;
            } else {
                std::fs::write(&bundle, b"")?;
            }
            let apply = SyncApply {
                project: project.into(),
                commit: commit.into(),
            };
            let (tx, reply) = self
                .client
                .0
                .client_streaming(apply, 8)
                .await
                .map_err(lost)?;
            let sent = files::send_file(&bundle, &tx).await;
            let _ = std::fs::remove_file(&bundle);
            sent?;
            drop(tx);
            reply.await.map_err(lost)??;
        }
        let present: Vec<PathBuf> = extra
            .iter()
            .filter(|f| root.join(f).exists())
            .cloned()
            .collect();
        for missing in extra.iter().filter(|f| !root.join(f).exists()) {
            said.push(format!(
                "ferry: {} из files проекта здесь нет — не везу",
                missing.display()
            ));
        }
        if !present.is_empty() {
            let dir = Dir::Project {
                name: project.into(),
                prefix: String::new(),
            };
            self.put(root, &present, dir).await?;
        }
        let present: Vec<String> = present.iter().map(|f| f.display().to_string()).collect();
        let present = if present.is_empty() {
            String::new()
        } else {
            format!(" + {}", present.join(", "))
        };
        said.push(format!(
            "ferry: {}:~/ferry/{project} = рабочее дерево здесь ({}){present}",
            self.name,
            &commit[..7.min(commit.len())]
        ));
        Ok(said.join("\n"))
    }

    /// Правки, сделанные на устройстве поверх снимка, — сюда, в рабочее дерево `root`.
    pub async fn pull(&self, root: &Path, project: &str) -> Reply<String> {
        let prepared = self
            .client
            .ask(SyncPrepare {
                project: project.into(),
                keep: String::new(),
            })
            .await?;
        if prepared.changes.is_empty() {
            return Ok(format!("ferry: на {} нет правок поверх снимка", self.name));
        }
        let saved = self.save_patch(project, &prepared.changes)?;
        let applied = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["apply", "--binary"])
            .arg(&saved)
            .status()?
            .success();
        if applied {
            Ok(format!(
                "ferry: правки с {} применены здесь (копия: {})",
                self.name,
                saved.display()
            ))
        } else {
            Err(Fail::new(format!(
                "правки с {} не легли чисто; патч: {} — попробуй git apply --3way",
                self.name,
                saved.display()
            )))
        }
    }

    /// Доверенные хосты этого устройства (`ferry trust`): git на том устройстве ходит к ним через
    /// наш ssh.
    fn trust(&self) -> Vec<String> {
        ferry_core::relay::trusted_in(&self.dir).unwrap_or_default()
    }

    fn save_patch(&self, project: &str, patch: &[u8]) -> Reply<PathBuf> {
        let dir = self.dir.join("saved");
        std::fs::create_dir_all(&dir)?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let path = dir.join(format!("{}-{project}-{now}.patch", self.name));
        std::fs::write(&path, patch)?;
        Ok(path)
    }

    /// Поток туннеля к `127.0.0.1:<port>` устройства.
    pub async fn tunnel(
        &self,
        port: u16,
    ) -> Reply<(mpsc::Sender<Vec<u8>>, mpsc::Receiver<Vec<u8>>)> {
        self.client
            .0
            .bidi_streaming(Tunnel { port }, 16, 16)
            .await
            .map_err(lost)
    }
}

/// Бандл со снимком `commit` под ссылкой `refs/ferry/snapshot`. Есть у устройства снимок `head`,
/// и он есть здесь — бандл только с недостающим.
fn make_bundle(root: &Path, commit: &str, head: Option<&str>, out: &Path) -> std::io::Result<()> {
    let git = |args: &[&str]| -> std::io::Result<std::process::Output> {
        Command::new("git").arg("-C").arg(root).args(args).output()
    };
    let ok = |out: std::process::Output| {
        if out.status.success() {
            Ok(())
        } else {
            Err(std::io::Error::other(format!(
                "git: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )))
        }
    };
    ok(git(&["update-ref", "refs/ferry/snapshot", commit])?)?;
    let mut args = vec![
        "bundle".to_string(),
        "create".into(),
        out.to_string_lossy().into_owned(),
        "refs/ferry/snapshot".into(),
    ];
    if let Some(head) = head
        && git(&["cat-file", "-e", &format!("{head}^{{commit}}")])?
            .status
            .success()
    {
        args.push(format!("^{head}"));
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    ok(git(&args)?)
}

/// Знает ли устройство запрос, появившийся в версии `since`. Устройство старше не разберёт его
/// и оборвёт связь — лучше сразу сказать, что его надо обновить.
fn need(name: &str, version: Option<&str>, since: &str, what: &str) -> Reply<()> {
    let parts = |v: &str| -> Vec<u32> { v.split('.').map(|p| p.parse().unwrap_or(0)).collect() };
    match version {
        Some(version) if parts(version) < parts(since) => Err(Fail::new(format!(
            "на {name} ferry {version}, а {what} — с {since}: обнови его — ferry update {name}"
        ))),
        _ => Ok(()),
    }
}

/// С какой версии устройство понимает этот ввод — и как его назвать в просьбе обновиться.
fn input_since(args: &[String], os: Option<&str>) -> Option<(&'static str, &'static str)> {
    let first = args.first().map_or("", String::as_str);
    let has = |flag: &str| args.iter().any(|a| a == flag);
    let elements = first == "ui" || first == "probe" || has("--name");
    Some(match first {
        "text" | "probe-text" => ("0.1.13", "текст на экране"),
        _ if has("--text") => ("0.1.13", "текст на экране"),
        "drag" => ("0.1.11", "ferry drag"),
        "clip" => ("0.1.11", "ferry clip"),
        "click" | "move" if has("--hold") => ("0.1.11", "--hold"),
        "key" if has("--for") => ("0.1.11", "key --for"),
        _ if elements && os == Some("macos") => ("0.1.10", "элементы окон на маке"),
        "focus" => ("0.1.9", "ferry focus"),
        "probe" | "probe-window" => ("0.1.8", "ferry wait"),
        "window" => ("0.1.6", "ferry shot --window"),
        _ if elements => ("0.1.4", "ferry ui и --name"),
        _ => return None,
    })
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> std::io::Result<T> + Send + 'static,
) -> Reply<T> {
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result.map_err(Fail::from),
        Err(e) => Err(Fail::new(format!("упало: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::{input_since, need};

    #[test]
    fn input_needs_the_version_that_knows_it() {
        let since = |args: &[&str], os| {
            let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
            input_since(&args, os).map(|(v, _)| v)
        };
        let (mac, win) = (Some("macos"), Some("windows"));
        assert_eq!(since(&["ui"], win), Some("0.1.4"));
        assert_eq!(
            since(&["ui"], mac),
            Some("0.1.10"),
            "на маке элементы — с 0.1.10"
        );
        assert_eq!(since(&["click", "--name", "Ок"], mac), Some("0.1.10"));
        assert_eq!(since(&["click", "--name", "Ок"], win), Some("0.1.4"));
        assert_eq!(since(&["probe", "Ок"], win), Some("0.1.8"));
        assert_eq!(since(&["probe", "Ок"], mac), Some("0.1.10"));
        assert_eq!(since(&["probe-window", "app"], mac), Some("0.1.8"));
        assert_eq!(since(&["focus", "app"], win), Some("0.1.9"));
        assert_eq!(since(&["window", "app"], win), Some("0.1.6"));
        assert_eq!(since(&["click", "1", "2"], mac), None);
        assert_eq!(since(&["drag", "1", "2", "3", "4"], win), Some("0.1.11"));
        assert_eq!(since(&["clip"], mac), Some("0.1.11"));
        assert_eq!(
            since(&["click", "1", "2", "--hold", "shift"], win),
            Some("0.1.11")
        );
        assert_eq!(since(&["key", "a", "--for", "1"], win), Some("0.1.11"));
        assert_eq!(since(&["key", "ctrl+s"], win), None);
        assert_eq!(since(&["text"], mac), Some("0.1.13"));
        assert_eq!(since(&["probe-text", "Готово"], win), Some("0.1.13"));
        assert_eq!(since(&["click", "--text", "Готово"], win), Some("0.1.13"));
    }

    #[test]
    fn older_device_is_asked_to_update() {
        let fail = need("win-10", Some("0.1.2"), "0.1.3", "ferry logs").unwrap_err();
        assert!(
            fail.message.contains("ferry update win-10"),
            "{}",
            fail.message
        );
        assert!(need("win-10", Some("0.1.3"), "0.1.3", "ferry logs").is_ok());
        assert!(need("win-10", Some("0.1.10"), "0.1.3", "ferry logs").is_ok());
        // Статус ещё не пришёл — пробуем: старое устройство просто оборвёт запрос.
        assert!(need("win-10", None, "0.1.3", "ferry logs").is_ok());
    }
}
