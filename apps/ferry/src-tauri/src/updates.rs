//! Один установщик для релизов и локальных подписанных пакетов.
use crate::{Reply, err};
use ferry_core::update::{MAX_PACKAGE, Monitor, Package};
use std::io::Read;
use std::path::PathBuf;
use tauri::{AppHandle, Manager};
use tauri_plugin_updater::{Update, UpdaterExt};

struct Install(Monitor);
impl Install {
    fn begin(app: &AppHandle) -> Reply<Self> {
        let monitor = app.state::<Monitor>().inner().clone();
        {
            let mut status = monitor.lock().unwrap();
            if status.busy {
                return Err("обновление уже выполняется".into());
            }
            status.busy = true;
            status.error = None;
        }
        Ok(Self(monitor))
    }
    fn schedule(self, app: &AppHandle, update: Update, bytes: Vec<u8>) -> String {
        let said = format!(
            "подпись проверена; ferry {} → {}: ставлю и перезапускаюсь",
            ferry_core::VERSION,
            update.version
        );
        let app = app.clone();
        std::thread::spawn(move || {
            // Ответ CLI должен уйти до остановки приложения.
            std::thread::sleep(std::time::Duration::from_secs(1));
            match update.install(&bytes) {
                Ok(()) => app.restart(),
                Err(e) => {
                    let message = format!("обновление не встало: {e}");
                    tracing::warn!("{message}");
                    self.0.lock().unwrap().error = Some(message);
                }
            }
        });
        said
    }
}
impl Drop for Install {
    fn drop(&mut self) {
        self.0.lock().unwrap().busy = false;
    }
}

pub async fn release(app: &AppHandle) -> Reply<String> {
    let install = Install::begin(app)?;
    let Some(update) = app.updater().map_err(err)?.check().await.map_err(err)? else {
        return Ok(format!("ferry {} — последняя версия", ferry_core::VERSION));
    };
    let bytes = update.download(|_, _| {}, || {}).await.map_err(err)?;
    Ok(install.schedule(app, update, bytes))
}

pub fn local(app: &AppHandle, file: PathBuf, signature: String) -> Reply<String> {
    let install = Install::begin(app)?;
    let package = Package::parse(&signature).map_err(err)?;
    package
        .check_target(&install.0.lock().unwrap())
        .map_err(err)?;
    let mut bytes = Vec::new();
    std::fs::File::open(file)
        .map_err(err)?
        .take(MAX_PACKAGE + 1)
        .read_to_end(&mut bytes)
        .map_err(err)?;
    if bytes.is_empty() || bytes.len() as u64 > MAX_PACKAGE {
        return Err("пакет должен быть от 1 байта до 512 МиБ".into());
    }
    let updater = app
        .updater_builder()
        // Явная локальная установка допускает новую сборку той же версии, но не откат.
        .version_comparator(|current, release| release.version >= current)
        .installer_arg("/S")
        .build()
        .map_err(err)?;
    // restore_update не ходит в сеть и обязательно проверяет подписанную версию.
    let update = updater
        .restore_update(serde_json::json!({
            "version": package.version,
            "url": "https://localhost/unused-local-package",
            "signature": package.signature,
        }))
        .map_err(err)?
        .ok_or_else(|| "локальная сборка старее установленной; откат не разрешён".to_string())?;
    update
        .verify(&bytes)
        .map_err(|e| format!("подпись пакета не прошла проверку: {e}"))?;
    #[cfg(target_os = "macos")]
    check_mac_package(&bytes)?;
    Ok(install.schedule(app, update, bytes))
}

/// У локального tar.gz в имени нет архитектуры. Проверяем оба исполняемых файла и
/// прежнюю подпись macOS до замены приложения, чтобы не потерять запуск и разрешения TCC.
#[cfg(target_os = "macos")]
fn check_mac_package(bytes: &[u8]) -> Reply<()> {
    use std::process::Command;
    let dir =
        ferry_net::files::temp(&ferry_core::ferry_home().map_err(err)?, "verify").map_err(err)?;
    std::fs::create_dir(&dir).map_err(err)?;
    let result = (|| {
        let archive = dir.join("package.tar.gz");
        std::fs::write(&archive, bytes).map_err(err)?;
        let output = Command::new("/usr/bin/tar")
            .arg("-xzf")
            .arg(&archive)
            .arg("-C")
            .arg(&dir)
            .output()
            .map_err(err)?;
        if !output.status.success() {
            return Err(format!(
                "не удалось распаковать Mac-пакет: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let staged = dir.join("ferry.app");
        let exe = std::env::current_exe().map_err(err)?;
        let installed = exe
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
            .ok_or("не удалось найти установленный ferry.app")?;
        let arch = if cfg!(target_arch = "aarch64") {
            "arm64"
        } else {
            "x86_64"
        };
        for name in ["ferry", "ferry-app"] {
            check_mac_arch(&staged.join("Contents/MacOS").join(name), arch)?;
        }
        let output = Command::new("/usr/bin/codesign")
            .args(["--verify", "--deep", "--strict"])
            .arg(&staged)
            .output()
            .map_err(err)?;
        if !output.status.success() {
            return Err("подпись macOS в пакете повреждена или отсутствует".into());
        }
        for child in ["", "Contents/MacOS/ferry"] {
            if mac_requirement(&staged.join(child))? != mac_requirement(&installed.join(child))? {
                return Err("сертификат macOS отличается от установленного ferry — пересобери с прежней подписью, чтобы сохранить разрешения".into());
            }
        }
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(dir);
    result
}

#[cfg(target_os = "macos")]
fn check_mac_arch(exe: &std::path::Path, arch: &str) -> Reply<()> {
    // Mach-O headers: mach-o/loader.h, mach-o/fat.h. Не требуем Xcode/lipo на узле.
    let mut header = Vec::new();
    std::fs::File::open(exe)
        .map_err(err)?
        .take(8 + 64 * 32)
        .read_to_end(&mut header)
        .map_err(err)?;
    let cpu = if arch == "arm64" {
        0x0100_000c
    } else {
        0x0100_0007
    };
    if mac_header_has_cpu(&header, cpu) {
        Ok(())
    } else {
        Err(format!(
            "Mac-пакет не содержит {} для {arch}",
            exe.file_name().unwrap_or_default().to_string_lossy()
        ))
    }
}

#[cfg(target_os = "macos")]
fn mac_header_has_cpu(header: &[u8], cpu: u32) -> bool {
    if header.len() < 8 {
        return false;
    }
    let (little, stride) = match &header[..4] {
        [0xcf, 0xfa, 0xed, 0xfe] => (true, 0),
        [0xfe, 0xed, 0xfa, 0xcf] => (false, 0),
        [0xca, 0xfe, 0xba, 0xbe] => (false, 20),
        [0xbe, 0xba, 0xfe, 0xca] => (true, 20),
        [0xca, 0xfe, 0xba, 0xbf] => (false, 32),
        [0xbf, 0xba, 0xfe, 0xca] => (true, 32),
        _ => return false,
    };
    let word = |offset| {
        let bytes = header[offset..offset + 4].try_into().unwrap();
        if little {
            u32::from_le_bytes(bytes)
        } else {
            u32::from_be_bytes(bytes)
        }
    };
    if stride == 0 {
        return word(4) == cpu;
    }
    let count = word(4) as usize;
    count <= 64
        && header.len() >= 8 + count * stride
        && (0..count).any(|i| word(8 + i * stride) == cpu)
}

#[cfg(target_os = "macos")]
fn mac_requirement(app: &std::path::Path) -> Reply<String> {
    let output = std::process::Command::new("/usr/bin/codesign")
        .args(["-d", "-r-"])
        .arg(app)
        .output()
        .map_err(err)?;
    if !output.status.success() {
        return Err("не удалось проверить прежний сертификат macOS".into());
    }
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    text.lines()
        .find(|line| line.starts_with("designated =>"))
        .map(str::to_owned)
        .ok_or_else(|| "нет требования подписи macOS".into())
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    #[test]
    fn reads_universal_headers_and_rejects_truncated_tables() {
        for (magic, stride) in [(0xcafebabe_u32, 20), (0xcafebabf_u32, 32)] {
            for little in [false, true] {
                let word = |n: u32| {
                    if little {
                        n.to_le_bytes()
                    } else {
                        n.to_be_bytes()
                    }
                };
                let mut header = vec![0; 8 + 2 * stride];
                header[..4].copy_from_slice(&word(magic));
                header[4..8].copy_from_slice(&word(2));
                header[8..12].copy_from_slice(&word(0x0100_0007));
                header[8 + stride..12 + stride].copy_from_slice(&word(0x0100_000c));
                assert!(super::mac_header_has_cpu(&header, 0x0100_0007));
                assert!(super::mac_header_has_cpu(&header, 0x0100_000c));
                assert!(!super::mac_header_has_cpu(&header, 99));
                assert!(!super::mac_header_has_cpu(&header[..12], 0x0100_0007));
                header[4..8].copy_from_slice(&word(u32::MAX));
                assert!(!super::mac_header_has_cpu(&header, 0x0100_0007));
            }
        }
    }

    #[test]
    fn rejects_a_mac_binary_without_the_required_architecture() {
        let exe = std::env::current_exe().unwrap();
        let (native, wrong) = if cfg!(target_arch = "aarch64") {
            ("arm64", "x86_64")
        } else {
            ("x86_64", "arm64")
        };
        super::check_mac_arch(&exe, native).unwrap();
        assert!(super::check_mac_arch(&exe, wrong).is_err());
    }
}
