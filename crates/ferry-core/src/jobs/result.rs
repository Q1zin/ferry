//! Ограниченный результат задачи: хвост лога или продолжение конкретного запуска.
use super::*;
use std::io::{Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicU64, Ordering};

pub const DEFAULT_BYTES: u32 = 16 * 1024;
pub const MAX_BYTES: u32 = 64 * 1024;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct JobResult {
    pub name: String,
    /// running, succeeded, failed, killed, lost.
    pub state: String,
    pub exit_code: Option<i32>,
    pub started: Option<u64>,
    pub cwd: String,
    pub run_id: String,
    pub log: String,
    pub cursor: String,
    pub more: bool,
    /// Сколько байтов пропущено при запросе хвоста без курсора.
    pub omitted_bytes: u64,
}

impl JobResult {
    pub fn command_code(&self) -> i32 {
        match self.state.as_str() {
            "running" | "succeeded" => 0,
            "failed" => self.exit_code.filter(|c| *c > 0 && *c <= 255).unwrap_or(1),
            _ => 1,
        }
    }
}

pub fn new_run_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "{}-{stamp}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

pub(super) fn generation(dir: &Path) -> io::Result<String> {
    match fs::read_to_string(dir.join("run-id")) {
        Ok(id) => Ok(id),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            // Старые задачи тоже читаются: на macOS/Windows дата создания лога меняется
            // при перезапуске, но не при дописывании. Не угадываем на других файловых системах.
            let born = fs::metadata(dir.join("log"))?
                .created()?
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(io::Error::other)?;
            Ok(format!("legacy-{}", born.as_nanos()))
        }
        Err(e) => Err(e),
    }
}

fn reported_state(state: &str, exit_code: Option<i32>) -> &'static str {
    match state {
        "running" => "running",
        "killed" => "killed",
        "lost" if exit_code.is_none() => "lost",
        _ if exit_code == Some(0) => "succeeded",
        _ => "failed",
    }
}

/// Не читает весь лог в память. Состояние смотрим до чтения лога, чтобы финальные
/// строки всегда попали в результат, который уже говорит «завершилась».
pub fn result_in(
    root: &Path,
    name: &str,
    cursor: Option<&str>,
    max_bytes: u32,
) -> io::Result<JobResult> {
    validate_name(name)?;
    result_at(&root.join("jobs").join(name), name, cursor, max_bytes)
}

pub(super) fn result_at(
    dir: &Path,
    name: &str,
    cursor: Option<&str>,
    max_bytes: u32,
) -> io::Result<JobResult> {
    if !(4..=MAX_BYTES).contains(&max_bytes) {
        return Err(io::Error::other("--max-bytes: от 4 до 65536"));
    }
    if !dir.is_dir() {
        return Err(io::Error::other(format!("нет задачи {name}")));
    }
    let run_id = generation(dir)?;
    let state = code(dir);
    let exit_code = read(dir, "exit").and_then(|s| s.parse::<i32>().ok());
    let state = if state == "killed" && running(dir) {
        "running"
    } else {
        reported_state(&state, exit_code)
    }
    .to_string();
    let mut file = fs::File::open(dir.join("log"))?;
    let len = file.metadata()?.len();
    let mut offset = match cursor {
        Some(cursor) => {
            let (id, at) = cursor
                .rsplit_once(':')
                .ok_or_else(|| io::Error::other("неверный курсор результата"))?;
            if id != run_id {
                return Err(io::Error::other(
                    "задача перезапущена; запроси result без --cursor",
                ));
            }
            let at = at
                .parse::<u64>()
                .map_err(|_| io::Error::other("неверный курсор результата"))?;
            if at > len {
                return Err(io::Error::other(
                    "лог стал короче курсора; запроси result без --cursor",
                ));
            }
            at
        }
        None => len.saturating_sub(max_bytes.into()),
    };
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    file.take(u64::from(max_bytes).min(len - offset))
        .read_to_end(&mut bytes)?;
    if cursor.is_none() && offset > 0 {
        let skip = bytes
            .iter()
            .take(3)
            .take_while(|b| **b & 0xc0 == 0x80)
            .count();
        bytes.drain(..skip);
        offset += skip as u64;
    }
    let final_bytes = state != "running" && offset + bytes.len() as u64 == len;
    let end = if final_bytes {
        bytes.len()
    } else {
        complete_utf8(&bytes)
    };
    let next = offset + end as u64;
    let result = JobResult {
        name: name.into(),
        state,
        exit_code,
        started: read(dir, "started").and_then(|t| t.parse().ok()),
        cwd: read(dir, "cwd").unwrap_or_default(),
        run_id: run_id.clone(),
        log: String::from_utf8_lossy(&bytes[..end]).into_owned(),
        cursor: format!("{run_id}:{next}"),
        more: next < len,
        omitted_bytes: if cursor.is_none() { offset } else { 0 },
    };
    if generation(dir)? != run_id {
        return Err(io::Error::other(
            "задача перезапущена во время чтения; запроси result без --cursor",
        ));
    }
    Ok(result)
}

/// Неполный UTF-8 символ оставляем следующему запросу; испорченные байты заменит lossy.
fn complete_utf8(bytes: &[u8]) -> usize {
    let mut offset = 0;
    while offset < bytes.len() {
        match std::str::from_utf8(&bytes[offset..]) {
            Ok(_) => return bytes.len(),
            Err(e) => match e.error_len() {
                Some(n) => offset += e.valid_up_to() + n,
                None => return offset + e.valid_up_to(),
            },
        }
    }
    bytes.len()
}

#[cfg(test)]
mod tests {
    #[test]
    fn exit_published_during_poll_overrides_transient_lost_state() {
        assert_eq!(super::reported_state("lost", Some(0)), "succeeded");
        assert_eq!(super::reported_state("lost", Some(7)), "failed");
        assert_eq!(super::reported_state("lost", None), "lost");
        assert_eq!(super::reported_state("killed", Some(0)), "killed");
    }

    use super::*;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("ferry-result-{}", new_run_id()));
            let dir = root.join("jobs/test");
            fs::create_dir_all(&dir).unwrap();
            for (name, value) in [
                ("run-id", "first"),
                ("exit", "7"),
                ("started", "1"),
                ("cwd", "/work"),
            ] {
                fs::write(dir.join(name), value).unwrap();
            }
            Self(root)
        }
        fn write(&self, name: &str, value: impl AsRef<[u8]>) {
            fs::write(self.0.join("jobs/test").join(name), value).unwrap();
        }
        fn result(&self, cursor: Option<&str>, max: u32) -> JobResult {
            result_in(&self.0, "test", cursor, max).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn tail_and_cursor_are_bounded_and_keep_the_real_exit() {
        let f = Fixture::new();
        f.write("log", "0123456789ERROR");
        let tail = f.result(None, 5);
        assert_eq!(tail.log, "ERROR");
        assert_eq!(tail.omitted_bytes, 10);
        assert_eq!(tail.state, "failed");
        assert_eq!(tail.command_code(), 7);
        assert_eq!(f.result(Some(&tail.cursor), 5).log, "");
        f.write("log", "0123456789ERRORmore data");
        let next = f.result(Some(&tail.cursor), 5);
        assert_eq!(next.log, "more ");
        assert!(next.more);
        assert_eq!(next.omitted_bytes, 0);
        assert_eq!(f.result(Some(&next.cursor), 5).log, "data");
        f.write("run-id", "second");
        assert!(
            result_in(&f.0, "test", Some(&tail.cursor), 5)
                .unwrap_err()
                .to_string()
                .contains("перезапущена")
        );
    }
    #[test]
    fn unicode_survives_chunk_boundaries_and_partial_writes() {
        let f = Fixture::new();
        f.write("log", "a😀бвend");
        let one = f.result(Some("first:0"), 4);
        assert_eq!(one.log, "a");
        let two = f.result(Some(&one.cursor), 4);
        assert_eq!(two.log, "😀");
        assert_eq!(f.result(None, 5).log, "вend");
        assert_eq!(complete_utf8(&[0xff, 0xf0, 0x9f]), 1);
        assert_eq!(complete_utf8(&[0xf0, 0x9f]), 0);
        f.write("log", [0xf0, 0x9f]);
        assert!(
            !f.result(None, 4).more,
            "terminal malformed UTF-8 must not stall"
        );
    }
    #[test]
    fn validates_offsets_names_limits_and_legacy_runs() {
        let f = Fixture::new();
        f.write("log", "hello");
        for cursor in ["first:99", "first:no", "bad"] {
            assert!(result_in(&f.0, "test", Some(cursor), 4).is_err());
        }
        for size in [0, 3, MAX_BYTES + 1] {
            assert!(result_in(&f.0, "test", None, size).is_err());
        }
        assert!(result_in(&f.0, "../test", None, 4).is_err());
        fs::remove_file(f.0.join("jobs/test/run-id")).unwrap();
        let legacy = f.result(None, 4);
        assert!(legacy.run_id.starts_with("legacy-"));
        assert_eq!(f.result(Some(&legacy.cursor), 4).log, "");
        f.write("killed", "");
        assert_eq!(f.result(None, 4).state, "killed");
        assert_eq!(f.result(None, 4).command_code(), 1);
    }
}
