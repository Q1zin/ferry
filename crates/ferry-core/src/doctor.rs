//! `ferry doctor <узел>`: готов ли узел собирать этот проект.
//!
//! Требования берутся из самого репозитория — `rust-toolchain.toml` и `Cargo.toml`, lock-файлы
//! JS, `git@`-адреса зависимостей — и из `tools`/`files` в настройках проекта на хабе. Каждая
//! проверка идёт дважды: в ssh-сессии и на рабочем столе (`ferry desk`) — у демона окружение
//! со времени его запуска, и поставленный позже инструмент он не увидит до `ferry restart`.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;
use std::process::Command;

use crate::sh_quote;

struct Check {
    key: String,
    label: String,
    script: String,
    hint: String,
}

impl Check {
    fn new(key: &str, label: &str, script: &str, hint: &str) -> Self {
        Check {
            key: key.into(),
            label: label.into(),
            script: script.into(),
            hint: hint.into(),
        }
    }
}

pub fn run(host: &str, repo: Option<(&Path, &str)>) -> io::Result<i32> {
    let checks = checks(host, repo)?;
    let env = crate::relay::node_env()?;
    let script = |desk: bool| {
        let mut script = format!(
            "{env}chk() {{ out=$(eval \"$2\" 2>&1); rc=$?; \
             printf '%s\\t%s\\t%s\\n' \"$1\" \"$rc\" \"$(printf '%s' \"$out\" | tr '\\t\\r' '  ' | head -1)\"; }}\n"
        );
        for check in &checks {
            if desk && check.key == "desk" {
                continue;
            }
            script += &format!("chk {} {}\n", sh_quote(&check.key), sh_quote(&check.script));
        }
        script
    };
    let guarded = |script: String| format!("{}{script}", crate::mode::GUARD);
    let ssh = results(Command::new("ssh").args([host, &guarded(script(false))]))?;
    let desk = if ssh.get("desk").is_some_and(|(ok, _)| *ok) {
        let desk = format!("ferry desk {}", sh_quote(&script(true)));
        Some(results(Command::new("ssh").args([host, &guarded(desk)]))?)
    } else {
        None
    };

    let width = checks
        .iter()
        .map(|c| c.label.chars().count())
        .max()
        .unwrap_or(0);
    let mut healthy = true;
    for check in &checks {
        let (ok, text) = ssh
            .get(&check.key)
            .cloned()
            .unwrap_or((false, "нет ответа".into()));
        let text = match (text.is_empty(), ok) {
            (true, true) => "есть".into(),
            (true, false) => "нет".into(),
            _ => text,
        };
        let on_desk = desk
            .as_ref()
            .and_then(|d| d.get(&check.key))
            .map(|(ok, _)| *ok);
        let (mark, note) = match (ok, on_desk) {
            (true, None | Some(true)) => ("✓", text),
            (true, Some(false)) => (
                "!",
                format!(
                    "{text} — но не на рабочем столе: у демона старое окружение, ferry restart {host}"
                ),
            ),
            (false, Some(true)) => ("!", format!("только на рабочем столе — {}", check.hint)),
            (false, _) => ("✗", format!("{text} — {}", check.hint)),
        };
        healthy &= mark == "✓";
        println!("  {mark} {:<width$}  {note}", check.label);
    }
    Ok(if healthy { 0 } else { 1 })
}

fn checks(host: &str, repo: Option<(&Path, &str)>) -> io::Result<Vec<Check>> {
    let mut checks = vec![
        Check::new("os", "система", "uname -s", ""),
        Check::new(
            "disk",
            "свободно на диске",
            "df -h . | awk 'NR==2 {print $4}'",
            "",
        ),
        Check::new("git", "git", "git --version", "https://git-scm.com"),
        Check::new(
            "desk",
            "рабочий стол",
            "ferry desk true",
            &format!("демон не отвечает: ferry restart {host}"),
        ),
    ];
    let Some((root, name)) = repo else {
        return Ok(checks);
    };
    let files = tracked(root)?;

    if let Some(channel) = rust_channel(root) {
        // Без нужного канала — что стоит: «есть только stable» понятнее, чем «нет».
        let script = format!(
            "if ! command -v rustup >/dev/null; then echo 'нет rustup (https://rustup.rs)'; false; \
             elif rustup toolchain list | grep -q '^{channel}-'; then rustup run {channel} rustc --version; \
             else echo \"есть только $(rustup toolchain list | cut -d' ' -f1 | tr '\\n' ' ')\"; false; fi"
        );
        let hint = format!("rustup toolchain install {channel}");
        checks.push(Check::new(
            "rust",
            &format!("rust {channel}"),
            &script,
            &hint,
        ));
    } else if files
        .iter()
        .any(|f| f == "Cargo.toml" || f.ends_with("/Cargo.toml"))
    {
        checks.push(Check::new(
            "rust",
            "rust",
            "cargo --version",
            "https://rustup.rs",
        ));
    }
    if checks.iter().any(|c| c.key == "rust") {
        // Rust на Windows линкуется MSVC; на других системах проверка просто проходит.
        let script = "case $(uname -s) in MINGW*|MSYS*) \
            '/c/Program Files (x86)/Microsoft Visual Studio/Installer/vswhere.exe' -latest \
            -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 \
            -property displayName | grep . ;; *) echo 'не нужен' ;; esac";
        let hint = "Visual Studio Build Tools с «Разработкой классических приложений на C++»: \
                    https://visualstudio.microsoft.com/visual-cpp-build-tools/";
        checks.push(Check::new("msvc", "MSVC (Windows)", script, hint));
    }

    for tool in js_tools(root, &files) {
        checks.push(tool_check(tool));
    }
    for tool in crate::project::load(name)?.tools {
        checks.push(tool_check(&tool));
    }
    let trusted = crate::relay::trusted()?;
    for (git_host, url) in git_hosts(root, &files) {
        let hint = if trusted.contains(&git_host) {
            "через релей не прошло — ошибка слева".to_string()
        } else {
            format!("у узла нет доступа: ferry trust {git_host}")
        };
        let script = format!("git ls-remote {} HEAD", sh_quote(&url));
        checks.push(Check::new(
            &format!("git:{git_host}"),
            &format!("доступ к {git_host}"),
            &script,
            &hint,
        ));
    }
    for file in crate::project::load(name)?.files {
        let script = format!(
            "test -e {} && echo есть",
            sh_quote(&format!("ferry/{name}/{file}"))
        );
        let hint = format!("он в files проекта — привезёт ferry sync {host}");
        checks.push(Check::new(&format!("file:{file}"), &file, &script, &hint));
    }
    Ok(checks)
}

fn tool_check(tool: &str) -> Check {
    let hint = match tool {
        "bun" => {
            "Windows: powershell -c \"irm bun.sh/install.ps1 | iex\"; иначе curl -fsSL https://bun.sh/install | bash"
        }
        "node" | "npm" => "https://nodejs.org",
        "pnpm" | "yarn" => "corepack enable (идёт с Node.js)",
        "cmake" => {
            "https://cmake.org/download (или cmake из C++-компонента Visual Studio — добавь в PATH)"
        }
        "ninja" => {
            "https://github.com/ninja-build/ninja/releases — ninja.exe в PATH; на macOS brew install ninja"
        }
        "ffmpeg" => "https://ffmpeg.org/download.html — в PATH",
        _ => "не найден в PATH",
    };
    let script = format!(
        "command -v {tool} >/dev/null && {{ {tool} --version 2>&1 | head -1; true; }}",
        tool = sh_quote(tool)
    );
    Check::new(tool, tool, &script, hint)
}

/// Канал Rust из `rust-toolchain.toml` (или старого `rust-toolchain`).
fn rust_channel(root: &Path) -> Option<String> {
    if let Ok(text) = fs::read_to_string(root.join("rust-toolchain.toml")) {
        let line = text
            .lines()
            .find(|l| l.trim_start().starts_with("channel"))?;
        return Some(line.split('"').nth(1)?.to_string());
    }
    let text = fs::read_to_string(root.join("rust-toolchain")).ok()?;
    Some(text.trim().to_string()).filter(|c| !c.is_empty())
}

/// Чем ставятся JS-зависимости: `packageManager` из package.json, иначе по lock-файлу.
fn js_tools(root: &Path, files: &[String]) -> Vec<&'static str> {
    let Ok(package) = fs::read_to_string(root.join("package.json")) else {
        return Vec::new();
    };
    let declared = package
        .split("\"packageManager\"")
        .nth(1)
        .and_then(|rest| rest.split('"').nth(1))
        .map(|spec| spec.split('@').next().unwrap_or_default().to_string());
    let has = |name: &str| files.iter().any(|f| f == name);
    let manager = match declared.as_deref() {
        Some("bun") => "bun",
        Some("pnpm") => "pnpm",
        Some("yarn") => "yarn",
        Some(_) => "npm",
        None if has("bun.lock") || has("bun.lockb") => "bun",
        None if has("pnpm-lock.yaml") => "pnpm",
        None if has("yarn.lock") => "yarn",
        None => "npm",
    };
    if manager == "bun" {
        vec!["bun"]
    } else {
        vec!["node", manager]
    }
}

/// Хосты `git@…` из манифестов и lock-файлов — с одним адресом каждого для проверки.
fn git_hosts(root: &Path, files: &[String]) -> Vec<(String, String)> {
    const MANIFESTS: [&str; 10] = [
        "Cargo.lock",
        "Cargo.toml",
        "package.json",
        "bun.lock",
        "package-lock.json",
        "pnpm-lock.yaml",
        "yarn.lock",
        "go.mod",
        "pyproject.toml",
        "requirements.txt",
    ];
    let mut hosts = BTreeMap::new();
    for file in files {
        let base = file.rsplit('/').next().unwrap_or(file);
        if !MANIFESTS.contains(&base) {
            continue;
        }
        let Ok(text) = fs::read_to_string(root.join(file)) else {
            continue;
        };
        for (host, url) in git_urls(&text) {
            hosts.entry(host).or_insert(url);
        }
    }
    hosts.into_iter().collect()
}

/// `git+ssh://git@host/path.git?…#…` и `git@host:path` из текста — как (хост, адрес для git).
fn git_urls(text: &str) -> Vec<(String, String)> {
    let stop = |c: char| c.is_whitespace() || "\"'(),=<>".contains(c);
    let mut urls = Vec::new();
    for (at, _) in text.match_indices("git@") {
        let start = text[..at].rfind(stop).map_or(0, |i| i + 1);
        let end = at
            + text[at..]
                .find(|c: char| stop(c) || "?#".contains(c))
                .unwrap_or(text.len() - at);
        let host = text[at + 4..end]
            .split([':', '/'])
            .next()
            .unwrap_or_default();
        if !host.contains('.')
            || !host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || ".-".contains(c))
        {
            continue;
        }
        let url = &text[start..end];
        urls.push((
            host.to_string(),
            url.strip_prefix("git+").unwrap_or(url).to_string(),
        ));
    }
    urls
}

fn tracked(root: &Path) -> io::Result<Vec<String>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("ls-files")
        .output()?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(String::from)
        .collect())
}

/// Строки `ключ<TAB>код<TAB>текст` от скрипта на узле.
fn results(command: &mut Command) -> io::Result<BTreeMap<String, (bool, String)>> {
    let out = command.output()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut results = BTreeMap::new();
    for line in text.lines() {
        let mut fields = line.splitn(3, '\t');
        if let (Some(key), Some(code), Some(text)) = (fields.next(), fields.next(), fields.next()) {
            results.insert(key.to_string(), (code == "0", text.trim().to_string()));
        }
    }
    if results.is_empty() {
        let why = String::from_utf8_lossy(&out.stderr);
        return Err(io::Error::other(format!("узел не ответил: {}", why.trim())));
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_git_hosts_in_lock_files() {
        let cargo =
            r#"source = "git+ssh://git@git.example.ru/dev/tauri-cef.git?branch=feat%2Fcef#abc123""#;
        let npm =
            r#""dep": "git+ssh://git@github.com/org/repo.git#v1", "x": "git@gitlab.com:a/b.git""#;
        assert_eq!(
            git_urls(cargo),
            [(
                "git.example.ru".into(),
                "ssh://git@git.example.ru/dev/tauri-cef.git".into()
            )]
        );
        assert_eq!(
            git_urls(npm),
            [
                (
                    "github.com".into(),
                    "ssh://git@github.com/org/repo.git".into()
                ),
                ("gitlab.com".into(), "git@gitlab.com:a/b.git".into()),
            ]
        );
        assert!(git_urls("email: someone at git@localhost").is_empty());
    }
}
