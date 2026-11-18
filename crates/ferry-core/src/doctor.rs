//! `ferry doctor <цели>`: готово ли устройство собирать этот проект.
//!
//! Требования берутся из самого репозитория — `rust-toolchain.toml` и `Cargo.toml`, lock-файлы
//! JS, `git@`-адреса зависимостей — и из `tools`/`files` в настройках проекта.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;
use std::process::Command;

use crate::{desk::Shell, sh_quote};

pub struct Check {
    label: String,
    probe: Probe,
    hint: String,
}

enum Probe {
    Shell,
    Os,
    Disk,
    Tool(String),
    Rust(String),
    Git(String),
    File(String),
    Msvc,
    Sdk,
}

impl Check {
    fn new(label: &str, probe: Probe, hint: &str) -> Self {
        Check {
            label: label.into(),
            probe,
            hint: hint.into(),
        }
    }
}

/// Отчёт по выполненным проверкам: строки для человека и всё ли в порядке.
pub fn report(checks: &[Check], results: &Results) -> (Vec<String>, bool) {
    let width = checks
        .iter()
        .map(|c| c.label.chars().count())
        .max()
        .unwrap_or(0);
    let mut lines = Vec::new();
    let mut healthy = true;
    for (id, check) in checks.iter().enumerate() {
        let (code, text) = results
            .get(&id)
            .cloned()
            .unwrap_or((1, "нет ответа".into()));
        let ok = code == 0;
        let text = match (code, text.is_empty()) {
            (127, _) => "не найден в PATH оболочки".into(),
            (0, true) => "есть".into(),
            (_, true) => "нет".into(),
            _ => text,
        };
        let (mark, note) = if ok {
            ("✓", text)
        } else {
            (
                "✗",
                if check.hint.is_empty() || code == 126 {
                    text
                } else {
                    format!("{text} — {}", check.hint)
                },
            )
        };
        healthy &= ok;
        lines.push(format!("  {mark} {:<width$}  {note}", check.label));
    }
    (lines, healthy)
}

/// Проверки для устройства `device`: из репозитория (`root`, `name`) и настроек проекта.
pub fn checks(
    device: &str,
    repo: Option<(&Path, &str)>,
    windows: bool,
) -> io::Result<(Vec<Check>, Option<std::path::PathBuf>)> {
    let mut checks = vec![
        Check::new("оболочка", Probe::Shell, ""),
        Check::new("система", Probe::Os, ""),
        Check::new("свободно на диске", Probe::Disk, ""),
        tool_check("git"),
    ];
    let Some((root, name)) = repo else {
        return Ok((checks, None));
    };
    let (profile_path, profile) = crate::project::for_repo(root, name)?;
    let files = tracked(root)?;
    let has_rust = files
        .iter()
        .any(|f| f == "Cargo.toml" || f.ends_with("/Cargo.toml"));
    if let Some(channel) = rust_channel(root) {
        validate_name(&channel)?;
        checks.push(tool_check("rustup"));
        let hint = format!("rustup toolchain install {channel}");
        checks.push(Check::new(
            &format!("rust {channel}"),
            Probe::Rust(channel),
            &hint,
        ));
    }
    if has_rust || checks.iter().any(|c| matches!(c.probe, Probe::Rust(_))) {
        checks.push(tool_check("cargo"));
        if windows {
            let hint = "Visual Studio Build Tools с «Разработкой классических приложений на C++»";
            checks.push(Check::new("MSVC (Windows)", Probe::Msvc, hint));
            checks.push(Check::new("Windows SDK", Probe::Sdk, hint));
        }
    }

    for tool in js_tools(root, &files)
        .into_iter()
        .chain(profile.tools.iter().map(String::as_str))
    {
        validate_name(tool)?;
        if !checks
            .iter()
            .any(|c| matches!(&c.probe, Probe::Tool(name) if name == tool))
        {
            checks.push(tool_check(tool));
        }
    }
    let trusted = crate::relay::trusted()?;
    for (git_host, url) in git_hosts(root, &files) {
        let hint = if trusted.contains(&git_host) {
            "через релей не прошло — ошибка слева".to_string()
        } else {
            format!("у устройства нет доступа: ferry trust {git_host} здесь")
        };
        checks.push(Check::new(
            &format!("доступ к {git_host}"),
            Probe::Git(url),
            &hint,
        ));
    }
    for file in profile.files {
        let hint = format!("он в files проекта — привезёт ferry sync {device}");
        checks.push(Check::new(
            &file,
            Probe::File(format!("ferry/{name}/{file}")),
            &hint,
        ));
    }
    Ok((checks, profile_path))
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
        "git" => "https://git-scm.com",
        "cargo" | "rustup" => "https://rustup.rs",
        _ => "не найден в PATH",
    };
    Check::new(tool, Probe::Tool(tool.into()), hint)
}

fn validate_name(name: &str) -> io::Result<()> {
    if !name.is_empty()
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._+-".contains(&c))
    {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "некорректное имя инструмента или канала Rust: {name}"
        )))
    }
}

pub type Results = BTreeMap<usize, (i32, String)>;

/// Идёт через обычный ferry run с окружением git-релея.
pub fn script(checks: &[Check], shell: Shell) -> String {
    let mut script = match shell {
        Shell::Cmd => String::new(),
        Shell::Powershell => POWERSHELL_CHECK.into(),
        _ => String::from(
            "chk() { out=$(eval \"$2\" 2>&1); rc=$?; \
             printf 'FERRY_DOCTOR\\t%s\\t%s\\t%s\\n' \"$1\" \"$rc\" \"$(printf '%s' \"$out\" | tr '\\t\\r' '  ' | head -1)\"; }\n",
        ),
    };
    for (id, check) in checks.iter().enumerate() {
        match shell {
            Shell::Cmd => {
                let command = match &check.probe {
                    Probe::Shell => "echo %COMSPEC%".into(),
                    Probe::Tool(tool) => format!("where.exe {tool} 2>nul"),
                    Probe::Rust(channel) => format!("rustup run {channel} rustc --version 2>&1"),
                    _ => continue,
                };
                if !script.is_empty() {
                    script.push_str(" & ");
                }
                script += &format!(
                    "echo FERRY_DOCTOR {id} & {command} & if errorlevel 1 (echo FERRY_FAIL) else (echo FERRY_OK)"
                );
            }
            Shell::Powershell => {
                if let Some(body) = powershell(&check.probe) {
                    script += &format!("chk {id} {{ {body} }};\n");
                }
            }
            _ => {
                if let Some(body) = bash(&check.probe) {
                    script += &format!("chk {id} {}\n", sh_quote(&body));
                }
            }
        }
    }
    script
}

fn bash(probe: &Probe) -> Option<String> {
    Some(match probe {
        Probe::Shell => "printf '%s\\n' \"$0\"".into(),
        Probe::Os => "uname -s".into(),
        Probe::Disk => "df -h . | awk 'NR==2 {print $4}'".into(),
        Probe::Tool(tool) => format!(
            "path=$(command -v {tool}) || exit 127; version=$({tool} --version 2>&1 | head -1); \
             printf '%s%s\\n' \"$path\" \"${{version:+ · $version}}\"",
            tool = sh_quote(tool)
        ),
        Probe::Rust(channel) => format!("rustup run {} rustc --version", sh_quote(channel)),
        Probe::Git(url) => format!("git ls-remote {} HEAD", sh_quote(url)),
        Probe::File(path) => format!("test -e {} && echo есть", sh_quote(path)),
        Probe::Msvc | Probe::Sdk => return None,
    })
}

const POWERSHELL_CHECK: &str = r#"
function chk($id, [scriptblock]$body) {
    $global:LASTEXITCODE = 0
    try {
        $output = @(& $body 2>&1)
        $code = $LASTEXITCODE
        $line = $output | Select-Object -First 1
    } catch {
        $code = 1
        $line = $_.Exception.Message
    }
    "FERRY_DOCTOR`t$id`t$code`t" + (([string]$line -split '\r?\n', 2)[0] -replace "`t", ' ')
}
"#;

fn ps_quote(text: &str) -> String {
    // PowerShell разбирает типографские одинарные кавычки как обычные.
    let mut literal = String::from("'");
    for c in text.chars() {
        literal.push(c);
        if matches!(c, '\'' | '‘' | '’' | '‚' | '‛') {
            literal.push(c);
        }
    }
    literal.push('\'');
    literal
}

fn powershell(probe: &Probe) -> Option<String> {
    Some(match probe {
        Probe::Shell => "(Get-Process -Id $PID).Path".into(),
        Probe::Os => "[Environment]::OSVersion.VersionString".into(),
        Probe::Disk => "('{0:N1} GiB' -f ((Get-PSDrive -Name $PWD.Drive.Name).Free / 1GB))".into(),
        Probe::Tool(tool) => format!(
            "$path = Get-Command {tool} -CommandType Application,ExternalScript -ErrorAction SilentlyContinue | Select-Object -First 1; \
             if (!$path) {{ $global:LASTEXITCODE = 127; return }}; \
             $version = & $path.Source --version 2>&1 | Select-Object -First 1; \
             $global:LASTEXITCODE = 0; if ($version) {{ \"$($path.Source) · $version\" }} else {{ $path.Source }}",
            tool = ps_quote(tool)
        ),
        Probe::Rust(channel) => format!("& rustup run {} rustc --version", ps_quote(channel)),
        Probe::Git(url) => format!("& git ls-remote {} HEAD", ps_quote(url)),
        Probe::File(path) => format!(
            "if (!(Test-Path -LiteralPath {})) {{ throw 'нет файла' }}",
            ps_quote(path)
        ),
        Probe::Msvc | Probe::Sdk => return None,
    })
}

const MSVC: &str = r#"
$base = [Environment]::GetFolderPath('ProgramFilesX86')
$vswhere = Join-Path $base 'Microsoft Visual Studio\Installer\vswhere.exe'
if (!(Test-Path -LiteralPath $vswhere)) { throw 'vswhere.exe не найден' }
$vs = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath | Select-Object -First 1
if (!$vs) { throw 'не установлен компонент Microsoft.VisualStudio.Component.VC.Tools.x86.x64' }
$cl = Get-ChildItem -LiteralPath (Join-Path $vs 'VC\Tools\MSVC') -Directory -ErrorAction SilentlyContinue |
    ForEach-Object { Join-Path $_.FullName 'bin\Hostx64\x64\cl.exe' } |
    Where-Object { Test-Path -LiteralPath $_ } | Select-Object -Last 1
if (!$cl) { throw "не найден компилятор cl.exe в $vs" }
$cl
"#;

const SDK: &str = r#"
$kits = (Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows Kits\Installed Roots' -ErrorAction SilentlyContinue).KitsRoot10
if (!$kits) { $kits = (Get-ItemProperty 'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows Kits\Installed Roots' -ErrorAction SilentlyContinue).KitsRoot10 }
if (!$kits) { $kits = $env:WindowsSdkDir }
if (!$kits) { throw 'не найден каталог Windows SDK 10/11' }
$sdk = Get-ChildItem -LiteralPath (Join-Path $kits 'Include') -Directory -ErrorAction SilentlyContinue |
    Where-Object {
        $lib = Join-Path $kits "Lib\$($_.Name)"
        (Test-Path -LiteralPath (Join-Path $_.FullName 'um\Windows.h')) -and
        (Test-Path -LiteralPath (Join-Path $_.FullName 'ucrt\stdio.h')) -and
        (Test-Path -LiteralPath (Join-Path $lib 'um\x64\kernel32.lib')) -and
        (Test-Path -LiteralPath (Join-Path $lib 'ucrt\x64\ucrt.lib'))
    } | Sort-Object Name -Descending | Select-Object -First 1
if (!$sdk) { throw "в $kits нет полной версии SDK с заголовками и библиотеками для x64" }
$sdk.FullName
"#;

/// Дополнительные данные Windows из реестра и файловой системы.
pub fn windows_script(checks: &[Check], shell: Shell, actual: &Results) -> Option<String> {
    let mut script = String::from(POWERSHELL_CHECK);
    for (id, check) in checks.iter().enumerate() {
        let body = match &check.probe {
            Probe::Msvc => MSVC.into(),
            Probe::Sdk => SDK.into(),
            Probe::Tool(tool) if actual.get(&id).is_some_and(|r| r.0 == 127) => {
                // Из свежего PATH программу только ищем: сборка ещё не видит тот же файл.
                let types = if shell == Shell::Powershell {
                    "Application,ExternalScript"
                } else {
                    "Application"
                };
                let tool = ps_quote(tool);
                format!(
                    r#"
                    $oldPath = $env:Path
                    $old = Get-Command {tool} -CommandType {types} -ErrorAction SilentlyContinue
                    try {{
                        $machine = [Environment]::GetEnvironmentVariable('Path', 'Machine')
                        $user = [Environment]::GetEnvironmentVariable('Path', 'User')
                        $env:Path = [Environment]::ExpandEnvironmentVariables("$machine;$user")
                        $fresh = Get-Command {tool} -CommandType {types} -ErrorAction SilentlyContinue | Select-Object -First 1
                    }} finally {{ $env:Path = $oldPath }}
                    if (!$old -and $fresh) {{
                        $global:LASTEXITCODE = 126
                        "найден в свежем PATH: $($fresh.Source) · перезапусти ferry на устройстве"
                    }} else {{
                        $global:LASTEXITCODE = 127
                    }}
                "#
                )
            }
            Probe::Os | Probe::Disk | Probe::File(_) if shell == Shell::Cmd => {
                powershell(&check.probe).unwrap()
            }
            Probe::Git(url) if shell == Shell::Cmd => {
                let git = checks
                    .iter()
                    .position(|c| matches!(&c.probe, Probe::Tool(tool) if tool == "git"));
                match git.and_then(|i| actual.get(&i)) {
                    Some((0, path)) => {
                        format!("& {} ls-remote {} HEAD", ps_quote(path), ps_quote(url))
                    }
                    _ => "throw 'git не найден в cmd'".into(),
                }
            }
            _ => continue,
        };
        script += &format!("chk {id} {{ {body} }};\n");
    }
    (script.len() > POWERSHELL_CHECK.len()).then_some(script)
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

/// Строки с результатами проверок. Cmd печатает путь отдельно от метки и кода.
pub fn results(checks: &[Check], output: &str, shell: Shell) -> Results {
    let mut results = Results::new();
    let mut cmd_probe = None;
    let mut cmd_text = None;
    for line in output.lines() {
        if shell == Shell::Cmd {
            let line = line.trim();
            if let Some(id) = line.strip_prefix("FERRY_DOCTOR ") {
                cmd_probe = id.parse::<usize>().ok().filter(|id| *id < checks.len());
                cmd_text = None;
            } else if let Some(id) = cmd_probe {
                match line {
                    "FERRY_OK" | "FERRY_FAIL" => {
                        let code = match line {
                            "FERRY_OK" => 0,
                            _ if matches!(checks[id].probe, Probe::Tool(_)) => 127,
                            _ => 1,
                        };
                        results.insert(id, (code, cmd_text.take().unwrap_or_default()));
                        cmd_probe = None;
                    }
                    _ if !line.is_empty() && cmd_text.is_none() => cmd_text = Some(line.to_owned()),
                    _ => {}
                }
            }
        } else if let Some(line) = line.strip_prefix("FERRY_DOCTOR\t") {
            let mut fields = line.splitn(3, '\t');
            let id = fields.next().and_then(|id| id.parse::<usize>().ok());
            let code = fields.next().and_then(|code| code.parse::<i32>().ok());
            if let (Some(id), Some(code), Some(text)) = (id, code, fields.next())
                && id < checks.len()
            {
                results.insert(id, (code, text.trim().to_string()));
            }
        }
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_paths_missing_tools_and_stale_path_separately() {
        let checks = vec![tool_check("git"), tool_check("cargo"), tool_check("bun")];
        let mut actual = results(
            &checks,
            "FERRY_DOCTOR 0 \r\nC:\\Program Files\\Git\\cmd\\git.exe\r\nFERRY_OK \r\n\
             FERRY_DOCTOR 1 \r\nFERRY_FAIL\r\nFERRY_DOCTOR 2 \r\nFERRY_FAIL\r\n",
            Shell::Cmd,
        );
        assert_eq!(actual[&0], (0, r"C:\Program Files\Git\cmd\git.exe".into()));
        assert_eq!(actual[&1].0, 127);
        let supplement = windows_script(&checks, Shell::Cmd, &actual).unwrap();
        assert!(supplement.contains("Get-Command 'cargo'"));
        assert!(!supplement.contains("Get-Command 'git'"));
        actual.extend(results(
            &checks,
            "FERRY_DOCTOR\t1\t126\tнайден в свежем PATH: cargo.exe · перезапусти ferry на устройстве\n\
             FERRY_DOCTOR\t2\t127\t\n",
            Shell::Powershell,
        ));
        let (lines, healthy) = report(&checks, &actual);
        assert!(!healthy);
        assert!(lines[0].contains(r"C:\Program Files\Git\cmd\git.exe"));
        assert!(lines[1].contains("перезапусти ferry"));
        assert!(!lines[1].contains("rustup.rs"));
        assert!(lines[2].contains("не найден в PATH оболочки"));
    }

    #[cfg(unix)]
    #[test]
    fn bash_reports_its_own_path_and_tool_resolution() {
        let checks = vec![
            Check::new("оболочка", Probe::Shell, ""),
            tool_check("git"),
            tool_check("ferry_absent_tool_8462"),
        ];
        let output = crate::desk::shell_with(&script(&checks, Shell::Bash), Shell::Bash)
            .unwrap()
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let actual = results(
            &checks,
            &String::from_utf8_lossy(&output.stdout),
            Shell::Bash,
        );
        assert!(actual[&0].1.ends_with("bash"));
        assert!(actual[&1].1.contains("git version"));
        assert_eq!(actual[&2].0, 127);
    }

    #[cfg(windows)]
    #[test]
    fn windows_shells_report_which_programs_they_resolve() {
        let checks = vec![
            Check::new("оболочка", Probe::Shell, ""),
            tool_check("cmd.exe"),
            tool_check("ferry_absent_tool_8462"),
        ];
        for shell in [Shell::Cmd, Shell::Powershell] {
            let output = crate::desk::shell_with(&script(&checks, shell), shell)
                .unwrap()
                .output()
                .unwrap();
            let actual = results(&checks, &String::from_utf8_lossy(&output.stdout), shell);
            assert_eq!(actual.get(&0).map(|row| row.0), Some(0), "{output:?}");
            assert!(actual[&1].1.to_lowercase().contains("cmd.exe"));
            assert_eq!(actual[&2].0, 127, "{output:?}");
        }
    }

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
