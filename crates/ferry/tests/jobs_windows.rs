//! Настоящий процесс Windows: установка меняет исходный exe, задача продолжает работать.
#![cfg(windows)]

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn ferry() -> PathBuf {
    std::env::var_os("FERRY_TEST_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_ferry")))
}

struct Home(PathBuf);

impl Drop for Home {
    fn drop(&mut self) {
        let _ = Command::new(ferry())
            .arg("stop")
            .env("USERPROFILE", &self.0)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn wait(done: impl Fn() -> bool) {
    let until = Instant::now() + Duration::from_secs(30);
    while !done() {
        assert!(Instant::now() < until, "задача не ответила за 30 с");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn background_job_survives_replacing_the_installed_executable() {
    let home = Home(std::env::temp_dir().join(format!("ferry-job-update-{}", std::process::id())));
    let _ = fs::remove_dir_all(&home.0);
    fs::create_dir_all(&home.0).unwrap();
    let installed = home.0.join("ferry.exe");
    fs::copy(ferry(), &installed).unwrap();
    let out = Command::new(&installed)
        .args([
            "job",
            "survive",
            "printf before; while [ ! -f continue ]; do sleep .05; done; ferry version || exit 9; printf after; exit 7",
        ])
        .env("USERPROFILE", &home.0)
        .env("FERRY_SHELL", r"C:\Program Files\Git\bin\bash.exe")
        .current_dir(&home.0)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let job = home.0.join(".ferry/jobs/survive");
    wait(|| fs::read_to_string(job.join("log")).is_ok_and(|s| s == "before"));
    let pid = fs::read(job.join("pid")).unwrap();

    // Работающий exe на Windows удалить нельзя: старый надсмотрщик мешал бы установке.
    fs::remove_file(&installed).unwrap();
    fs::copy(ferry(), &installed).unwrap();
    fs::write(home.0.join("continue"), b"").unwrap();
    wait(|| job.join("exit").exists());
    assert_eq!(fs::read_to_string(job.join("exit")).unwrap(), "7");
    assert_eq!(
        fs::read_to_string(job.join("log")).unwrap(),
        format!("beforeferry {}\nafter", env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(fs::read(job.join("pid")).unwrap(), pid);
    assert_eq!(fs::read(&installed).unwrap(), fs::read(ferry()).unwrap());
}
