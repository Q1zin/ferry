//! Owned-window recording: one continuous stream, bounded frame spool, explicit ffmpeg result.
use crate::devices::finish;
use base64::Engine;
use ferry_core::{
    control::{self, Request, Response},
    device::Fail,
    screen::live::OwnedWindow,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};
const MAX_BYTES: u64 = 128 * 1024 * 1024;
const MAX_FRAMES: usize = 18000;
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Options {
    pub device: Option<String>,
    pub job: String,
    pub run: String,
    pub window: String,
    pub fps: u32,
    pub max_seconds: u32,
}
fn root(id: &str) -> Result<PathBuf, Fail> {
    if id.is_empty()
        || id.len() > 80
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
    {
        return Err(Fail::new("capture id: 1..80 ASCII букв/цифр/_/-"));
    }
    Ok(ferry_core::ferry_home()?.join("captures").join(id))
}
fn write(path: &Path, value: &impl Serialize) -> Result<(), Fail> {
    ferry_core::write_private(path, &serde_json::to_string(value).map_err(Fail::new)?)?;
    Ok(())
}
fn read<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, Fail> {
    if fs::metadata(path)?.len() > 16 * 1024 * 1024 {
        return Err(Fail::new("capture state слишком большой"));
    }
    serde_json::from_slice(&fs::read(path)?).map_err(Fail::new)
}
fn lock(dir: &Path) -> Result<fs::File, Fail> {
    Ok(fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join("worker.lock"))?)
}
fn active(dir: &Path) -> Result<bool, Fail> {
    match lock(dir)?.try_lock() {
        Ok(()) => Ok(false),
        Err(fs::TryLockError::WouldBlock) => Ok(true),
        Err(fs::TryLockError::Error(e)) => Err(e.into()),
    }
}
fn terminal(v: &Value) -> bool {
    matches!(v["state"].as_str(), Some("completed" | "failed"))
}
pub(crate) fn status(id: &str) -> Result<Value, Fail> {
    let dir = root(id)?;
    let _: Options = read(&dir.join("request.json"))?;
    let mut value: Value =
        read(&dir.join("report.json")).unwrap_or(json!({"state":"starting","complete":false}));
    let live = active(&dir)?;
    if !live && !terminal(&value) {
        value["state"] = "interrupted".into();
        value["complete"] = false.into();
    }
    value["id"] = id.into();
    value["worker_active"] = live.into();
    value["manifest_file"] = dir
        .join("manifest.json")
        .to_string_lossy()
        .to_string()
        .into();
    Ok(value)
}
fn spawn(dir: &Path, id: &str) -> Result<(), Fail> {
    if active(dir)? {
        return Ok(());
    }
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("worker.log"))?;
    let exe = ferry_core::jobs::prepare_supervisor(
        &ferry_core::ferry_home()?,
        &std::env::current_exe()?,
    )?;
    let mut cmd = Command::new(exe);
    cmd.args(["capture", "work", id])
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x08000000 | 0x00000200);
    let mut child = cmd.spawn()?;
    for _ in 0..100 {
        if active(dir)? || child.try_wait()?.is_some() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Err(Fail::new("capture worker не подтвердил запуск"))
}
pub(crate) fn start(id: &str, options: Options) -> Result<Value, Fail> {
    let request = ferry_core::screen::live::Request {
        fps: options.fps,
        max_width: 1280,
        quality: 70,
        target: Some(OwnedWindow {
            job: options.job.clone(),
            run: options.run.clone(),
            title: options.window.clone(),
        }),
    };
    request.validate()?;
    if !(1..=3600).contains(&options.max_seconds) {
        return Err(Fail::new("max-seconds: 1..3600"));
    }
    let dir = root(id)?;
    fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    }
    let init = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join("init.lock"))?;
    init.lock()?;
    if dir.join("request.json").exists() {
        let saved: Options = read(&dir.join("request.json"))?;
        if saved != options {
            return Err(Fail::new(
                "capture id уже связан с другим окном/запуском/параметрами",
            ));
        }
        if terminal(&status(id)?) {
            return status(id);
        }
    } else {
        write(&dir.join("request.json"), &options)?;
        write(
            &dir.join("report.json"),
            &json!({"state":"starting","complete":false,"frames":0,"created":ferry_core::now()}),
        )?;
    }
    if !dir.join("report.json").exists() {
        write(
            &dir.join("report.json"),
            &json!({"state":if dir.join("frames.jsonl").exists(){"interrupted"}else{"starting"},"complete":false,"frames":0,"created":ferry_core::now()}),
        )?;
    }
    drop(init);
    spawn(&dir, id)?;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let value = status(id)?;
        if value["frames"].as_u64().unwrap_or(0) > 0
            || terminal(&value)
            || value["state"] == "interrupted"
        {
            return Ok(value);
        }
        if Instant::now() >= deadline {
            return Err(Fail::new(
                "capture: первый кадр ещё не подтверждён; используй status/stop с тем же id",
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
pub(crate) fn stop(id: &str) -> Result<Value, Fail> {
    let dir = root(id)?;
    let current = status(id)?;
    if terminal(&current) {
        return Ok(current);
    }
    ferry_core::write_private(&dir.join("stop"), "stop")?;
    spawn(&dir, id)?;
    let deadline = Instant::now() + Duration::from_secs(75);
    loop {
        let value = status(id)?;
        if !value["worker_active"].as_bool().unwrap_or(false) {
            return Ok(value);
        }
        if Instant::now() >= deadline {
            return Err(Fail::new(
                "capture ещё завершает кодирование; status/stop с тем же id",
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
fn wait_child(child: &mut std::process::Child, seconds: u64) -> Result<bool, Fail> {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status.success());
        }
        if Instant::now() >= deadline {
            ferry_core::desk::kill_tree(child.id()).ok();
            let _ = child.wait();
            return Err(Fail::new("ffmpeg превысил timeout"));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
fn encoder_available() -> Result<PathBuf, Fail> {
    let name = if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    };
    let mut paths = std::env::var_os("PATH")
        .map(|v| std::env::split_paths(&v).collect::<Vec<_>>())
        .unwrap_or_default();
    paths.extend([
        ferry_core::home()?.join(".local/bin"),
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
    ]);
    for path in paths {
        let path = path.join(name);
        if !path.is_file() {
            continue;
        }
        let mut child = match Command::new(&path)
            .args(["-version"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(_) => continue,
        };
        if wait_child(&mut child, 5)? {
            return Ok(path);
        }
    }
    Err(Fail::new(
        "нужен ffmpeg на пульте: PATH, ~/.local/bin, /opt/homebrew/bin или /usr/local/bin",
    ))
}
#[derive(Clone, Serialize, Deserialize)]
struct SavedFrame {
    file: String,
    sequence: u64,
    captured_ms: u64,
    width: u32,
    height: u32,
    bytes: u64,
}
fn frames(dir: &Path) -> Result<Vec<SavedFrame>, Fail> {
    let ledger = dir.join("frames.jsonl");
    if !ledger.exists() {
        return Ok(Vec::new());
    }
    if fs::metadata(&ledger)?.len() > 8 * 1024 * 1024 {
        return Err(Fail::new("слишком большой журнал кадров"));
    }
    let text = fs::read_to_string(ledger)?;
    let mut result: Vec<SavedFrame> = Vec::new();
    let mut bytes = 0u64;
    for line in text.split_inclusive('\n') {
        // The last write can be interrupted; only newline-committed frames are recoverable.
        if !line.ends_with('\n') {
            break;
        }
        if result.len() >= MAX_FRAMES {
            return Err(Fail::new("слишком много кадров"));
        }
        let row: SavedFrame = serde_json::from_str(line).map_err(Fail::new)?;
        if row.file != format!("frames/{:06}.jpg", result.len()) {
            return Err(Fail::new("неверный путь кадра"));
        }
        let meta = fs::symlink_metadata(dir.join(&row.file))?;
        bytes = bytes.saturating_add(row.bytes);
        if !meta.is_file()
            || meta.is_symlink()
            || meta.len() != row.bytes
            || row.bytes > 4 * 1024 * 1024
            || bytes > MAX_BYTES
            || result.last().is_some_and(|previous| {
                row.sequence <= previous.sequence || row.captured_ms < previous.captured_ms
            })
        {
            return Err(Fail::new("кадр повреждён или нарушены пределы/порядок"));
        }
        result.push(row);
    }
    Ok(result)
}
fn concat(rows: &[SavedFrame], fps: u32) -> String {
    let mut text = "ffconcat version 1.0\n".to_string();
    for (i, row) in rows.iter().enumerate() {
        let duration = rows
            .get(i + 1)
            .map(|next| next.captured_ms.saturating_sub(row.captured_ms).max(1) as f64 / 1000.0)
            .unwrap_or(1.0 / f64::from(fps));
        text += &format!("file '{}'\nduration {duration:.6}\n", row.file);
    }
    if let Some(last) = rows.last() {
        text += &format!("file '{}'\n", last.file);
    }
    text
}
fn encode(dir: &Path, rows: &[SavedFrame], fps: u32, encoder: &Path) -> Result<PathBuf, Fail> {
    if rows.is_empty() {
        return Err(Fail::new("ни одного подтверждённого кадра"));
    }
    fs::write(dir.join("frames.ffconcat"), concat(rows, fps))?;
    let log = fs::File::create(dir.join("encoder.log"))?;
    let mut child = Command::new(encoder)
        .current_dir(dir)
        .args([
            "-nostdin",
            "-y",
            "-loglevel",
            "error",
            "-nostats",
            "-f",
            "concat",
            "-safe",
            "1",
            "-i",
            "frames.ffconcat",
            "-vsync",
            "vfr",
            "-vf",
            "pad=ceil(iw/2)*2:ceil(ih/2)*2",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-movflags",
            "+faststart",
            "-fs",
            "134217728",
            "video.partial.mp4",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()
        .map_err(|e| Fail::new(format!("ffmpeg: {e}")))?;
    if !wait_child(&mut child, 60)? {
        return Err(Fail::new("ffmpeg не создал видео; см. encoder.log"));
    }
    let path = dir.join("video.partial.mp4");
    if fs::metadata(&path)?.len() == 0 || fs::metadata(&path)?.len() >= MAX_BYTES {
        return Err(Fail::new("пустое видео или достигнут предел 128 МиБ"));
    }
    fs::rename(path, dir.join("video.mp4"))?;
    Ok(dir.join("video.mp4"))
}
fn work(id: &str) -> Result<Value, Fail> {
    let dir = root(id)?;
    let guard = lock(&dir)?;
    if guard.try_lock().is_err() {
        return status(id);
    }
    let options: Options = read(&dir.join("request.json"))?;
    let prior: Value = read(&dir.join("report.json"))?;
    if terminal(&prior) {
        return Ok(prior);
    }
    let mut rows = frames(&dir)?;
    let mut bytes = rows.iter().map(|f| f.bytes).sum::<u64>();
    let mut error = None;
    let mut reason = "requested_stop";
    let started = Instant::now();
    let encoder = encoder_available();
    if prior["state"] != "starting" || !rows.is_empty() {
        error = Some(
            "контроллер записи был прерван; сохранены только подтверждённые кадры".to_string(),
        );
        reason = "interrupted";
    }
    if error.is_none() && rows.is_empty() && !dir.join("stop").exists() {
        let capture = (|| -> Result<(), Fail> {
            let encoder = encoder.as_ref().map_err(|e| Fail::new(e.to_string()))?;
            write(
                &dir.join("report.json"),
                &json!({"state":"connecting","complete":false,"frames":0,"worker_pid":std::process::id(),"encoder":encoder,"created":prior["created"]}),
            )?;
            fs::create_dir_all(dir.join("frames"))?;
            let mut ledger = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("frames.jsonl"))?;
            let request = Request::ScreenStream {
                device: options.device.clone(),
                request: ferry_core::screen::live::Request {
                    fps: options.fps,
                    max_width: 1280,
                    quality: 70,
                    target: Some(OwnedWindow {
                        job: options.job.clone(),
                        run: options.run.clone(),
                        title: options.window.clone(),
                    }),
                },
            };
            let (tx, rx) = mpsc::sync_channel(1);
            std::thread::spawn(move || {
                let answer = control::open(request);
                match answer {
                    Ok(answer) => {
                        for item in answer {
                            if tx.send(item).is_err() {
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                    }
                }
            });
            let mut last_frame = Instant::now();
            loop {
                if dir.join("stop").exists() {
                    break;
                }
                if started.elapsed().as_secs() >= u64::from(options.max_seconds) {
                    reason = "time_limit";
                    return Err(Fail::new("достигнут max-seconds; запись ограничена"));
                }
                let frame = match rx.recv_timeout(Duration::from_millis(200)) {
                    Ok(Ok(Response::Frame(f))) => f,
                    Ok(Ok(Response::Error(e))) => return Err(e),
                    Ok(Err(e)) => return Err(e.into()),
                    Ok(Ok(_)) => return Err(Fail::new("поток записи закончился")),
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if last_frame.elapsed().as_secs() > 20 {
                            return Err(Fail::new("поток не присылал кадров 20 секунд"));
                        }
                        continue;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        return Err(Fail::new("соединение потока оборвалось"));
                    }
                };
                if frame.jpeg.len() > 6 * 1024 * 1024 {
                    return Err(Fail::new("кадр больше допустимого размера"));
                }
                let jpeg = base64::engine::general_purpose::STANDARD
                    .decode(&frame.jpeg)
                    .map_err(Fail::new)?;
                if jpeg.len() > 4 * 1024 * 1024
                    || bytes.saturating_add(jpeg.len() as u64) > MAX_BYTES
                    || rows.len() >= MAX_FRAMES
                {
                    reason = "storage_limit";
                    return Err(Fail::new("предел записи: 128 МиБ / 18000 кадров"));
                }
                if rows.last().is_some_and(|r| {
                    frame.sequence <= r.sequence || frame.captured_ms < r.captured_ms
                }) {
                    return Err(Fail::new("нарушен порядок кадров"));
                }
                let row = SavedFrame {
                    file: format!("frames/{:06}.jpg", rows.len()),
                    sequence: frame.sequence,
                    captured_ms: frame.captured_ms,
                    width: frame.width,
                    height: frame.height,
                    bytes: jpeg.len() as u64,
                };
                fs::write(dir.join(&row.file), jpeg)?;
                writeln!(
                    ledger,
                    "{}",
                    serde_json::to_string(&row).map_err(Fail::new)?
                )?;
                ledger.flush()?;
                last_frame = Instant::now();
                bytes += row.bytes;
                rows.push(row);
                write(
                    &dir.join("report.json"),
                    &json!({"state":"recording","complete":false,"frames":rows.len(),"bytes":bytes,"worker_pid":std::process::id(),"created":prior["created"]}),
                )?;
            }
            Ok(())
        })();
        if let Err(e) = capture {
            error = Some(e.to_string());
        }
    }
    write(
        &dir.join("report.json"),
        &json!({"state":"finalizing","complete":false,"frames":rows.len(),"bytes":bytes,"error":error,"created":prior["created"]}),
    )?;
    let video = match encoder
        .as_ref()
        .map_err(|e| Fail::new(e.to_string()))
        .and_then(|encoder| encode(&dir, &rows, options.fps, encoder))
    {
        Ok(p) => Some(p),
        Err(e) => {
            error = Some(match error {
                Some(old) => format!("{old}; {e}"),
                None => e.to_string(),
            });
            None
        }
    };
    let value = json!({"schema_version":1,"id":id,"state":if error.is_none(){"completed"}else{"failed"},"complete":error.is_none(),"error":error,"reason":reason,"frames":rows.len(),"bytes":bytes,"created":prior["created"],"finished_at":ferry_core::now(),"video_file":video,"manifest_file":dir.join("manifest.json"),"frame_spool":dir.join("frames"),"encoder":encoder.ok(),"options":options});
    write(
        &dir.join("manifest.json"),
        &json!({"schema_version":1,"recording":value,"frames":rows}),
    )?;
    write(&dir.join("report.json"), &value)?;
    Ok(value)
}
pub fn run(args: &[&str]) -> io::Result<i32> {
    match args {
        [action @ ("status" | "stop" | "work"), id] => finish((|| {
            let value = match *action {
                "stop" => stop(id)?,
                "work" => work(id)?,
                _ => status(id)?,
            };
            println!("{value}");
            Ok(i32::from(
                value["state"] == "failed" || value["state"] == "interrupted",
            ))
        })()),
        ["start", device, tail @ ..] => {
            let (mut rest, mut job, mut run, mut window, mut id, mut fps, mut max_seconds) =
                (tail, None, None, None, None, 10, 300);
            while !rest.is_empty() {
                match rest {
                    ["--job", v, t @ ..] => {
                        job = Some(*v);
                        rest = t;
                    }
                    ["--run", v, t @ ..] => {
                        run = Some(*v);
                        rest = t;
                    }
                    ["--window", v, t @ ..] => {
                        window = Some(*v);
                        rest = t;
                    }
                    ["--id", v, t @ ..] => {
                        id = Some(*v);
                        rest = t;
                    }
                    ["--fps", v, t @ ..] => {
                        fps = v.parse().map_err(io::Error::other)?;
                        rest = t;
                    }
                    ["--max-seconds", v, t @ ..] => {
                        max_seconds = v.parse().map_err(io::Error::other)?;
                        rest = t;
                    }
                    _ => return crate::usage(),
                }
            }
            finish((|| {
                let value = start(
                    id.ok_or_else(|| Fail::new("нужен --id"))?,
                    Options {
                        device: (*device != "local").then(|| (*device).into()),
                        job: job.ok_or_else(|| Fail::new("нужен --job"))?.into(),
                        run: run.ok_or_else(|| Fail::new("нужен --run"))?.into(),
                        window: window.ok_or_else(|| Fail::new("нужен --window"))?.into(),
                        fps,
                        max_seconds,
                    },
                )?;
                println!("{value}");
                Ok(i32::from(
                    value["state"] == "failed" || value["state"] == "interrupted",
                ))
            })())
        }
        _ => crate::usage(),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn concat_preserves_frame_times_without_shell_interpolation() {
        let rows = [
            SavedFrame {
                file: "frames/000000.jpg".into(),
                sequence: 1,
                captured_ms: 1000,
                width: 10,
                height: 10,
                bytes: 3,
            },
            SavedFrame {
                file: "frames/000001.jpg".into(),
                sequence: 2,
                captured_ms: 1375,
                width: 10,
                height: 10,
                bytes: 3,
            },
        ];
        let text = concat(&rows, 10);
        assert!(text.contains("duration 0.375000"));
        assert!(text.contains("duration 0.100000"));
        assert_eq!(text.matches("frames/000001.jpg").count(), 2);
    }
    #[test]
    fn ids_cannot_escape_capture_storage() {
        assert!(root("../other").is_err());
        assert!(root("").is_err());
        assert!(root("valid-id_12").is_ok());
    }
    #[test]
    fn recovery_keeps_only_committed_frames_and_refuses_corruption() {
        let dir =
            std::env::temp_dir().join(format!("ferry-capture-{}", ferry_core::jobs::unique_id()));
        fs::create_dir_all(dir.join("frames")).unwrap();
        fs::write(dir.join("frames/000000.jpg"), b"jpg").unwrap();
        let row = SavedFrame {
            file: "frames/000000.jpg".into(),
            sequence: 1,
            captured_ms: 1000,
            width: 10,
            height: 10,
            bytes: 3,
        };
        let line = serde_json::to_string(&row).unwrap();
        fs::write(dir.join("frames.jsonl"), format!("{line}\n{{\"file\":")).unwrap();
        assert_eq!(frames(&dir).unwrap().len(), 1);
        fs::write(dir.join("frames/000000.jpg"), b"changed").unwrap();
        assert!(frames(&dir).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
}
