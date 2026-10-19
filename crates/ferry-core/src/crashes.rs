//! Падения программ на этой машине — для `ferry crashes`: какие и когда, разбор последнего.
//! macOS — отчёты `~/Library/Logs/DiagnosticReports/*.ips` (тип 309 — падение); Windows —
//! события «Application Error» журнала «Приложение»: отчёты WER лежат в ProgramData, и
//! пользователю их не прочесть.

use std::io;
#[cfg(target_os = "macos")]
use std::path::Path;

/// Падение: когда (секунды Unix), программа и где подробности (файл отчёта или путь программы).
#[derive(Debug, PartialEq)]
struct Crash {
    at: u64,
    process: String,
    place: String,
}

/// Падения, свежие сверху (до 20), или с `last` — разбор самого свежего. `process` — часть
/// имени программы, без учёта регистра; пусто — все.
pub fn report(process: &str, last: bool) -> io::Result<String> {
    let crashes = os::list(process)?;
    let whose = match process {
        "" => String::new(),
        _ => format!(" у «{process}»"),
    };
    if last {
        return Ok(match crashes.first() {
            Some(crash) => os::details(crash)?,
            None => format!("падений{whose} нет\n"),
        });
    }
    if crashes.is_empty() {
        return Ok(format!("падений{whose} нет\n"));
    }
    let now = crate::now();
    let mut text = String::new();
    for crash in crashes.iter().take(20) {
        let when = crate::jobs::ago(now.saturating_sub(crash.at));
        text += &format!("{when:<14} {:<24} {}\n", crash.process, crash.place);
    }
    Ok(text)
}

#[cfg(target_os = "macos")]
mod os {
    use super::{Crash, Path};
    use std::io::{self, BufRead};

    pub fn list(filter: &str) -> io::Result<Vec<Crash>> {
        list_in(
            &crate::home()?.join("Library/Logs/DiagnosticReports"),
            filter,
        )
    }

    /// Отчёты о падениях в каталоге: первая строка `.ips` — заголовок JSON с типом и программой.
    pub(super) fn list_in(dir: &Path, filter: &str) -> io::Result<Vec<Crash>> {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut crashes = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "ips") {
                continue;
            }
            let mut first = String::new();
            let Ok(file) = std::fs::File::open(&path) else {
                continue;
            };
            if io::BufReader::new(file).read_line(&mut first).is_err() {
                continue;
            }
            let Ok(head) = serde_json::from_str::<serde_json::Value>(&first) else {
                continue;
            };
            if head["bug_type"] != "309" {
                continue;
            }
            let process = head["app_name"]
                .as_str()
                .or(head["name"].as_str())
                .unwrap_or("?")
                .to_string();
            if !process.to_lowercase().contains(&filter.to_lowercase()) {
                continue;
            }
            let at = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs());
            crashes.push(Crash {
                at,
                process,
                place: path.display().to_string(),
            });
        }
        crashes.sort_by_key(|c| std::cmp::Reverse(c.at));
        Ok(crashes)
    }

    pub fn details(crash: &Crash) -> io::Result<String> {
        let text = std::fs::read_to_string(&crash.place)?;
        Ok(super::summary(&text) + &format!("отчёт целиком: {}\n", crash.place))
    }
}

/// Главное из отчёта `.ips`: программа и версия, исключение, сообщение библиотеки и стек
/// упавшего потока. Кадр без символа (релизная сборка без отладочной информации) — модуль и
/// смещение в нём: по ним символы найдёт `atos`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn summary(ips: &str) -> String {
    use serde_json::Value;
    let (head, body) = ips.split_once('\n').unwrap_or((ips, ""));
    let head: Value = serde_json::from_str(head).unwrap_or_default();
    let body: Value = serde_json::from_str(body).unwrap_or_default();
    let field = |v: &Value| v.as_str().unwrap_or_default().to_string();
    let mut text = format!(
        "{} {} — {}, {}\n",
        field(&head["app_name"]),
        field(&head["app_version"]),
        field(&head["timestamp"]),
        field(&head["os_version"]),
    );
    let exception = &body["exception"];
    let mut what = format!(
        "{} ({})",
        field(&exception["type"]),
        field(&exception["signal"])
    );
    if let Some(subtype) = exception["subtype"].as_str() {
        what += &format!(" {subtype}");
    }
    if let Some(indicator) = body["termination"]["indicator"].as_str() {
        what += &format!(": {indicator}");
    }
    text += &format!("{what}\n");
    if let Some(asi) = body["asi"].as_object() {
        for (library, said) in asi {
            for line in said.as_array().into_iter().flatten() {
                text += &format!("{library}: {}\n", field(line));
            }
        }
    }
    let images = body["usedImages"].as_array().cloned().unwrap_or_default();
    let Some(index) = body["faultingThread"].as_u64() else {
        return text;
    };
    let thread = &body["threads"][index as usize];
    let name = [&thread["name"], &thread["queue"]]
        .iter()
        .find_map(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    text += &format!("поток {index} {name} — упал:\n");
    for (i, frame) in thread["frames"]
        .as_array()
        .into_iter()
        .flatten()
        .take(30)
        .enumerate()
    {
        let image = frame["imageIndex"]
            .as_u64()
            .and_then(|i| images.get(i as usize))
            .and_then(|image| image["name"].as_str())
            .unwrap_or("?");
        // Сигнатуры C++ бывают на сотни знаков — хватит начала.
        let place = match frame["symbol"].as_str() {
            Some(symbol) => format!(
                "{} + {}",
                symbol.chars().take(160).collect::<String>(),
                frame["symbolLocation"].as_u64().unwrap_or(0)
            ),
            None => format!("+0x{:x}", frame["imageOffset"].as_u64().unwrap_or(0)),
        };
        text += &format!("{i:>4}  {image:<30} {place}\n");
    }
    text
}

#[cfg(windows)]
mod os {
    use super::Crash;
    use std::io;
    use std::process::Stdio;

    /// Последние 200 событий «Application Error»: по строке на падение — время, программа, путь;
    /// с `last` — текст самого свежего, как его пишет Windows.
    const SCRIPT: &str = r#"
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$mode = $args[0]; $filter = $args[1]
$events = @(Get-WinEvent -FilterHashtable @{LogName = 'Application'; ProviderName = 'Application Error'} -MaxEvents 200 -ErrorAction SilentlyContinue)
if ($filter) { $events = @($events | Where-Object { "$($_.Properties[0].Value)" -like "*$filter*" }) }
if ($mode -eq 'last') {
    if ($events.Count -gt 0) { $events[0].Message }
    exit 0
}
foreach ($e in $events) {
    "{0}`t{1}`t{2}" -f ([DateTimeOffset]$e.TimeCreated).ToUnixTimeSeconds(), $e.Properties[0].Value, $e.Properties[10].Value
}
"#;

    fn run(mode: &str, filter: &str) -> io::Result<String> {
        let out = crate::screen::powershell("ferry-crashes.ps1", SCRIPT)?
            .args([mode, filter])
            .stdin(Stdio::null())
            .output()?;
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        if !out.status.success() {
            let said = String::from_utf8_lossy(&out.stderr);
            return Err(io::Error::other(format!(
                "журнал событий не прочитался: {}",
                said.trim()
            )));
        }
        Ok(text)
    }

    pub fn list(filter: &str) -> io::Result<Vec<Crash>> {
        Ok(super::events(&run("list", filter)?))
    }

    pub fn details(crash: &Crash) -> io::Result<String> {
        let text = run("last", &crash.process)?;
        Ok(text.trim_end().to_string() + "\n")
    }
}

/// Строки скрипта Windows: `время<TAB>программа<TAB>путь`.
#[cfg_attr(not(windows), allow(dead_code))]
fn events(text: &str) -> Vec<Crash> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.trim_end_matches('\r').splitn(3, '\t');
            Some(Crash {
                at: fields.next()?.parse().ok()?,
                process: fields.next()?.to_string(),
                place: fields.next().unwrap_or_default().to_string(),
            })
        })
        .collect()
}

#[cfg(not(any(target_os = "macos", windows)))]
mod os {
    use super::Crash;
    use std::io;

    pub fn list(_: &str) -> io::Result<Vec<Crash>> {
        Err(io::Error::other("падения — только на macOS и Windows"))
    }

    pub fn details(_: &Crash) -> io::Result<String> {
        Err(io::Error::other("падения — только на macOS и Windows"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Выжимка настоящего отчёта ferry-app 0.1.9 (паника → abort на главном потоке).
    const IPS: &str = concat!(
        r#"{"app_name":"ferry-app","timestamp":"2026-10-19 22:47:54.00 +0700","app_version":"0.1.9","bug_type":"309","os_version":"macOS 26.4 (25E246)","name":"ferry-app"}"#,
        "\n",
        r#"{"exception":{"type":"EXC_CRASH","signal":"SIGABRT"},"termination":{"indicator":"Abort trap: 6"},"asi":{"libsystem_c.dylib":["abort() called"]},"faultingThread":0,"threads":[{"name":"main","queue":"com.apple.main-thread","frames":[{"imageOffset":36028,"symbol":"__pthread_kill","symbolLocation":8,"imageIndex":1},{"imageOffset":11598844,"imageIndex":0}]}],"usedImages":[{"name":"ferry-app"},{"name":"libsystem_kernel.dylib"}]}"#
    );

    #[test]
    fn ips_summary_has_the_crash_and_the_stack() {
        let text = summary(IPS);
        assert!(text.starts_with("ferry-app 0.1.9 — 2026-10-19 22:47:54.00 +0700, macOS 26.4"));
        assert!(
            text.contains("EXC_CRASH (SIGABRT): Abort trap: 6\n"),
            "{text}"
        );
        assert!(
            text.contains("libsystem_c.dylib: abort() called\n"),
            "{text}"
        );
        assert!(text.contains("поток 0 main — упал:\n"), "{text}");
        assert!(
            text.contains("libsystem_kernel.dylib         __pthread_kill + 8"),
            "{text}"
        );
        // Без символа — модуль и смещение.
        assert!(
            text.contains("ferry-app                      +0xb0fbfc"),
            "{text}"
        );

        // Бывают отчёты без потоков — только исключение.
        let bare = concat!(
            r#"{"app_name":"logarithmica","bug_type":"309"}"#,
            "\n",
            r#"{"exception":{"type":"EXC_BAD_ACCESS","signal":"SIGSEGV","subtype":"KERN_INVALID_ADDRESS at 0x0"},"reportNotes":["x"]}"#
        );
        let text = summary(bare);
        assert!(
            text.contains("EXC_BAD_ACCESS (SIGSEGV) KERN_INVALID_ADDRESS at 0x0\n"),
            "{text}"
        );
        assert!(!text.contains("поток"), "{text}");
        // Мусор вместо отчёта — не паника.
        summary("не json");
    }

    #[test]
    fn windows_event_lines_parse() {
        let text = "1791300205\tdeskflow-core.exe\tC:\\Program Files\\Deskflow\\deskflow-core.exe\r\n\
                    1791200000\tnotepad.exe\t\r\nмусор\n";
        let crashes = events(text);
        assert_eq!(crashes.len(), 2);
        assert_eq!(crashes[0].process, "deskflow-core.exe");
        assert_eq!(
            crashes[0].place,
            "C:\\Program Files\\Deskflow\\deskflow-core.exe"
        );
        assert_eq!(crashes[1].at, 1_791_200_000);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn only_crashes_of_the_named_program_are_listed() {
        let dir = std::env::temp_dir().join(format!("ferry-crashes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ferry-app-1.ips"), IPS).unwrap();
        std::fs::write(
            dir.join("ferry-app-2.ips"),
            "{\"app_name\":\"ferry-app\",\"bug_type\":\"288\"}\n{}",
        )
        .unwrap();
        std::fs::write(dir.join("Other-3.ips"), IPS.replace("ferry-app", "Other")).unwrap();
        std::fs::write(dir.join("notes.txt"), "x").unwrap();

        let all = os::list_in(&dir, "").unwrap();
        assert_eq!(all.len(), 2, "только падения (309) и только .ips: {all:?}");
        let ours = os::list_in(&dir, "FERRY").unwrap();
        assert_eq!(ours.len(), 1);
        assert!(ours[0].place.ends_with("ferry-app-1.ips"));
        assert!(os::list_in(&dir.join("нет"), "").unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
