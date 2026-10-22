use super::*;
use std::fs::{self, File};

const LIMIT: u64 = 64 * 1024 * 1024;

fn private_file(path: &Path) -> io::Result<File> {
    let mut options = fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

pub(super) struct Spool {
    stdout: File,
    stderr: File,
    bytes: u64,
    failure: Option<String>,
}
impl Spool {
    pub fn new(path: &Path) -> io::Result<Self> {
        fs::create_dir(path)?;
        Ok(Self {
            stdout: private_file(&path.join("stdout"))?,
            stderr: private_file(&path.join("stderr"))?,
            bytes: 0,
            failure: None,
        })
    }
    pub fn write(&mut self, data: &[u8], error: bool) {
        if self.failure.is_some() {
            return;
        }
        let result = if data.len() as u64 > LIMIT.saturating_sub(self.bytes) {
            Err(io::Error::other(
                "вывод устройства превысил 64 МиБ; поток остановлен",
            ))
        } else {
            self.bytes += data.len() as u64;
            if error {
                &mut self.stderr
            } else {
                &mut self.stdout
            }
            .write_all(data)
        };
        if let Err(e) = result {
            self.failure = Some(e.to_string());
        }
    }
    pub fn check(&self) -> Result<(), Fail> {
        self.failure.as_ref().map_or(Ok(()), |e| Err(Fail::new(e)))
    }
}

pub(super) fn each(
    names: &[String],
    job: impl Fn(&str, &Printer) -> Result<i32, Fail> + Sync,
) -> io::Result<i32> {
    let root = ferry_core::ferry_home()?
        .join("outputs")
        .join(format!("run-{}", ferry_core::jobs::unique_id()));
    fs::create_dir_all(&root)?;
    let active = File::create(root.join("worker.lock"))?;
    active.lock()?;
    let mut rows = Vec::new();
    for (batch_index, batch) in names.chunks(8).enumerate() {
        let completed = std::thread::scope(|scope| {
            let threads: Vec<_> = batch
                .iter()
                .enumerate()
                .map(|(i, name)| {
                    let path = root.join((batch_index * 8 + i).to_string());
                    let job = &job;
                    scope.spawn(move || {
                        let printer = Printer::spooled(&path)?;
                        let result =
                            job(name, &printer).and_then(|code| printer.check().map(|()| code));
                        let (code, error) = match result {
                            Ok(code) => (code, None),
                            Err(e) => (e.code, Some(e.to_string())),
                        };
                        Ok::<_, io::Error>((name.clone(), path, code, error))
                    })
                })
                .collect();
            threads
                .into_iter()
                .map(|thread| {
                    thread
                        .join()
                        .map_err(|_| io::Error::other("поток устройства прерван"))?
                })
                .collect::<io::Result<Vec<_>>>()
        })?;
        for (name, path, code, error) in &completed {
            rows.push(
                serde_json::json!({"device":name,"exit_code":code,"error":error,
                "stdout":path.join("stdout"),"stderr":path.join("stderr")}),
            );
        }
        ferry_core::write_private(
            &root.join("report.json"),
            &serde_json::json!({"complete":rows.len()==names.len(),"results":rows}).to_string(),
        )?;
        for (name, path, code, error) in completed {
            eprintln!("\n--- {name} · stdout ---");
            io::copy(
                &mut File::open(path.join("stdout"))?,
                &mut io::stdout().lock(),
            )?;
            eprintln!("\n--- {name} · stderr ---");
            io::copy(
                &mut File::open(path.join("stderr"))?,
                &mut io::stderr().lock(),
            )?;
            if let Some(message) = &error {
                eprintln!("ferry: {name}: {message}");
            }
            eprintln!("\n--- {name} · exit {code} ---");
        }
    }
    ferry_core::write_private(
        &root.join("report.json"),
        &serde_json::json!({"complete":true,"results":rows}).to_string(),
    )?;
    eprintln!(
        "ferry: сохранён вывод устройств: {}",
        root.join("report.json").display()
    );
    Ok(rows
        .iter()
        .filter_map(|r| r["exit_code"].as_i64())
        .find(|&code| code != 0)
        .unwrap_or(0) as i32)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn streams_are_separate_and_spool_limit_stops_further_writes() {
        let path =
            std::env::temp_dir().join(format!("ferry-spool-{}", ferry_core::jobs::unique_id()));
        let mut spool = Spool::new(&path).unwrap();
        spool.write(b"stdout\0", false);
        spool.write(b"stderr\n", true);
        assert_eq!(fs::read(path.join("stdout")).unwrap(), b"stdout\0");
        assert_eq!(fs::read(path.join("stderr")).unwrap(), b"stderr\n");
        spool.bytes = LIMIT;
        spool.write(b"overflow", false);
        assert!(spool.check().is_err());
        spool.write(b"ignored", true);
        drop(spool);
        assert_eq!(fs::read(path.join("stderr")).unwrap(), b"stderr\n");
        fs::remove_dir_all(path).unwrap();
    }
}
