//! Локальные подписанные сборки: метаданные до передачи и состояние перезапуска.
use crate::device::Fail;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::{Arc, Mutex};

pub const MAX_PACKAGE: u64 = 512 * 1024 * 1024;
pub type Monitor = Arc<Mutex<Status>>;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub boot: String,
    pub version: String,
    pub os: String,
    pub arch: String,
    pub busy: bool,
    pub error: Option<String>,
}
impl Default for Status {
    fn default() -> Self {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        Self {
            boot: format!("{}-{stamp}", std::process::id()),
            version: crate::VERSION.into(),
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            busy: false,
            error: None,
        }
    }
}
impl Status {
    pub fn restarted(&self, before: &Self, version: &str) -> bool {
        self.boot != before.boot && self.version == version && !self.busy && self.error.is_none()
    }
}

pub struct Package {
    pub signature: String,
    pub version: String,
    pub os: &'static str,
    pub arch: Option<&'static str>,
}
impl Package {
    pub fn read(file: &Path) -> Result<Self, Fail> {
        check_size(file)?;
        let mut sig = file.as_os_str().to_owned();
        sig.push(".sig");
        let signature = std::fs::read_to_string(Path::new(&sig)).map_err(|e| {
            Fail::new(format!(
                "нужна подпись рядом с пакетом ({}.sig): {e}",
                file.display()
            ))
        })?;
        Self::parse(&signature)
    }
    /// Это только разбор, НЕ проверка подписи. Перед установкой Tauri проверяет весь пакет,
    /// trusted comment и привязку версии своим встроенным публичным ключом.
    pub fn parse(signature: &str) -> Result<Self, Fail> {
        let bad = || {
            Fail::new(
                "нужна подпись Tauri с версией и именем пакета; пересобери пакет актуальным Tauri CLI",
            )
        };
        if signature.len() > 16384 {
            return Err(bad());
        }
        let decoded = STANDARD.decode(signature.trim()).map_err(|_| bad())?;
        let text = std::str::from_utf8(&decoded).map_err(|_| bad())?;
        let comment = text
            .lines()
            .find_map(|line| line.strip_prefix("trusted comment: "))
            .ok_or_else(bad)?;
        let field = |key: &str| comment.split('\t').find_map(|part| part.strip_prefix(key));
        let version = field("version:")
            .filter(|v| !v.is_empty())
            .ok_or_else(bad)?
            .to_owned();
        let file = field("file:").ok_or_else(bad)?;
        let (os, arch) = if file == "ferry.app.tar.gz" {
            ("macos", None)
        } else if file.starts_with("ferry_") && file.ends_with("_x64-setup.exe") {
            ("windows", Some("x86_64"))
        } else if file.starts_with("ferry_") && file.ends_with("_arm64-setup.exe") {
            ("windows", Some("aarch64"))
        } else {
            return Err(Fail::new(
                "ожидается подписанный ferry.app.tar.gz или ferry_*_x64/arm64-setup.exe",
            ));
        };
        Ok(Self {
            signature: signature.trim().into(),
            version,
            os,
            arch,
        })
    }
    pub fn check_target(&self, status: &Status) -> Result<(), Fail> {
        if self.os != status.os || self.arch.is_some_and(|arch| arch != status.arch) {
            return Err(Fail::new(format!(
                "пакет для {} {}, устройство — {} {}",
                self.os,
                self.arch.unwrap_or("macOS"),
                status.os,
                status.arch
            )));
        }
        Ok(())
    }
}
pub fn check_size(file: &Path) -> Result<(), Fail> {
    let meta = std::fs::metadata(file)?;
    if !meta.is_file() || meta.len() == 0 || meta.len() > MAX_PACKAGE {
        return Err(Fail::new(
            "пакет должен быть непустым файлом не больше 512 МиБ",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn signature(file: &str, version: &str) -> String {
        STANDARD.encode(format!("untrusted comment: test\nabc\ntrusted comment: timestamp:1\tfile:{file}\tversion:{version}\ndef\n"))
    }
    #[test]
    fn reads_signed_metadata_and_rejects_wrong_platform() {
        let package = Package::parse(&signature("ferry_0.1.19_x64-setup.exe", "0.1.19")).unwrap();
        assert_eq!(package.version, "0.1.19");
        let mut target = Status {
            os: "windows".into(),
            arch: "x86_64".into(),
            ..Status::default()
        };
        package.check_target(&target).unwrap();
        target.arch = "aarch64".into();
        assert!(package.check_target(&target).is_err());
        assert!(Package::parse("bad").is_err());
        assert!(Package::parse(&signature("other.exe", "0.1.19")).is_err());
        assert!(Package::parse(&signature("ferry.app.tar.gz", "")).is_err());
    }
    #[test]
    fn online_same_process_is_not_a_completed_update() {
        let before = Status::default();
        assert!(!before.restarted(&before, crate::VERSION));
        let mut after = before.clone();
        after.boot.push('x');
        assert!(after.restarted(&before, crate::VERSION));
        assert!(!after.restarted(&before, "99.0.0"));
        after.error = Some("failed".into());
        assert!(!after.restarted(&before, crate::VERSION));
    }
}
