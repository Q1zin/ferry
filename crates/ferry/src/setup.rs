//! Narrow installation for the two supported agents; existing foreign entries are preserved.
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};
const SKILL: &str = include_str!("../../../skills/ferry/SKILL.md");
const MARKER: &str = "<!-- Managed by ferry setup. -->\n";

// Exact released training files, including versions installed before the backup existed.
const RELEASED_SKILLS: &[&str] = &[
    "4ededb1c376163490a00cb222e8dd94fe09312784407e1baac704bd9c2e667c9",
    "00cb5edc34b0da079a3ef03e4d01e74d4ccdd9eb5d04382194fcbeb0d5e7e35b",
    "940afe96b7d7b42c05ab2dae49ca9b1ddd527fc659f0505736b1f33541b522ce",
    "f59b0a023f3812dd766ac0ea7bf7bd002e3a1cfa68effdd7904c05c94deb7c98",
    "168edeca0a87543c87141902a34ee7f243b887b9fd710bbacf325e29c768bd58",
    "172fcefb3b219b32a306b74ac97da523ad4b4358137bc0494f5b70d1a244d6a2",
    "d44425e6fb3b3953eeffd75d5ca33960b4b9a6d7b3c85a0cc9ebcac233be209b",
    "ff0da7c9116f548dfcdb8ad8ada5371f6ce54b4c030f19650b09f104e2fe8b16",
    "f8efe6a11d7234531b11ebf59c03523bf5909ac85761ef9ec2c0f271dd5d5313",
    "7da80148e3544f336fcade22254d061e379b55fbe8dbb131138e6865016f82cd",
];
fn executable(name: &str) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> =
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect();
    if let Ok(home) = ferry_core::home() {
        dirs.extend([
            home.join(".local/bin"),
            home.join(".cargo/bin"),
            home.join(".bun/bin"),
        ]);
    }
    #[cfg(target_os = "macos")]
    dirs.extend([
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/Applications/Codex.app/Contents/Resources"),
        PathBuf::from(
            "/Applications/ChatGPT.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS",
        ),
    ]);
    for dir in dirs {
        let path = dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        if path.is_file() {
            return Some(path);
        }
    }
    None
}
fn skill_path(agent: &str) -> io::Result<PathBuf> {
    Ok(ferry_core::home()?.join(match agent {
        "claude" => ".claude/skills/ferry/SKILL.md",
        "codex" => ".agents/skills/ferry/SKILL.md",
        _ => return Err(io::Error::other("агент: claude или codex")),
    }))
}
fn bounded_command(exe: &Path, args: &[&str]) -> io::Result<(bool, Vec<u8>)> {
    let mut command = Command::new(exe);
    command.args(args);
    let result = crate::mcp::capture_command(
        command,
        &std::sync::atomic::AtomicBool::new(false),
        Duration::from_secs(15),
    )?;
    if result.interrupted.is_some() || result.out_truncated || result.err_truncated {
        return Err(io::Error::other(
            "CLI агента превысил время или предел ответа",
        ));
    }
    Ok((result.code == 0, result.out))
}
fn config(agent: &str, exe: Option<&Path>) -> io::Result<Option<(String, Vec<String>)>> {
    let entry = if agent == "claude" {
        // Read only the ferry entry, never print the configuration or environment values.
        if std::env::var_os("CLAUDE_CONFIG_DIR").is_some() {
            return Err(io::Error::other(
                "CLAUDE_CONFIG_DIR задан: настрой MCP вручную в выбранном каталоге",
            ));
        }
        let path = ferry_core::home()?.join(".claude.json");
        if !path.exists() {
            return Ok(None);
        }
        if fs::metadata(&path)?.len() > 16 * 1024 * 1024 {
            return Err(io::Error::other("конфигурация Claude слишком большая"));
        }
        let v: Value = serde_json::from_slice(&fs::read(path)?)
            .map_err(|_| io::Error::other("не удалось разобрать конфигурацию Claude"))?;
        match v["mcpServers"].get("ferry") {
            Some(v) => v.clone(),
            None => return Ok(None),
        }
    } else {
        let Some(exe) = exe else { return Ok(None) };
        let (ok, bytes) = bounded_command(exe, &["mcp", "get", "ferry", "--json"])?;
        if !ok {
            return Ok(None);
        }
        let v: Value = serde_json::from_slice(&bytes)
            .map_err(|_| io::Error::other("Codex вернул неверную конфигурацию MCP"))?;
        v.get("transport").cloned().unwrap_or(v)
    };
    let command = entry["command"].as_str().unwrap_or_default().to_string();
    let args = entry["args"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    Ok(Some((command, args)))
}
fn ours(command: &str, args: &[String]) -> bool {
    args == ["mcp"]
        && Path::new(command)
            .file_name()
            .is_some_and(|s| s == "ferry" || s == "ferry.exe")
}
fn mcp_path_error(command: &str, current: &Path) -> Option<String> {
    let path = Path::new(command);
    let reason = if !path.is_absolute() {
        "MCP запускает ferry через PATH. VS Code, открытый отдельно от терминала, может не найти его."
    } else if !path.is_file() {
        "Записанный путь MCP больше не существует."
    } else if path.canonicalize().ok() != current.canonicalize().ok() {
        "MCP использует другой бинарник ferry."
    } else {
        return None;
    };
    Some(format!(
        "{reason} В настройке MCP укажи абсолютную команду {} с аргументом mcp.",
        current.display()
    ))
}

fn backup(agent: &str) -> io::Result<PathBuf> {
    Ok(ferry_core::ferry_home()?
        .join("setup")
        .join(format!("{agent}-skill.md")))
}

fn classify_skill(installed: &str, backup: Option<&str>) -> &'static str {
    if installed == SKILL {
        return "current";
    }
    let normalized = installed.replace("\r\n", "\n");
    let body = normalized.strip_prefix(MARKER).unwrap_or(&normalized);
    let hash = format!("{:x}", Sha256::digest(body.as_bytes()));
    if Some(installed) == backup || body == SKILL || RELEASED_SKILLS.contains(&hash.as_str()) {
        "upgrade_available"
    } else {
        "custom"
    }
}

fn skill_state(skill: &Path, backup: &Path) -> io::Result<&'static str> {
    match fs::read_to_string(skill) {
        Ok(installed) => Ok(classify_skill(
            &installed,
            fs::read_to_string(backup).ok().as_deref(),
        )),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok("missing"),
        Err(e) => Err(e),
    }
}
fn status() -> io::Result<Value> {
    let mut agents = Vec::new();
    let current = std::env::current_exe()?;
    for id in ["claude", "codex"] {
        let exe = executable(id);
        let configuration = config(id, exe.as_deref());
        let (configured, conflict, error) = match configuration {
            Ok(Some((command, args))) if ours(&command, &args) => {
                let problem = mcp_path_error(&command, &current);
                (problem.is_none(), false, problem)
            }
            Ok(Some(_)) => (false, true, None),
            Ok(None) => (false, false, None),
            Err(e) => (false, false, Some(e.to_string())),
        };
        let skill = skill_state(&skill_path(id)?, &backup(id)?).unwrap_or("unreadable");
        agents.push(json!({"id":id,"available":exe.is_some(),"command":exe,"skill_installed":skill!="missing","skill_state":skill,"mcp_configured":configured,"config_conflict":conflict,"error":error}));
    }
    let app_available = matches!(
        ferry_core::control::call_session_timeout(
            ferry_core::control::Request::State,
            "",
            Duration::from_secs(2)
        ),
        Ok(ferry_core::control::Response::State(_))
    );
    Ok(
        json!({"schema_version":1,"agents":agents,"cli_available":std::env::current_exe()?.is_file(),"app_available":app_available}),
    )
}
fn install(agent: &str) -> io::Result<Value> {
    let skill = skill_path(agent)?;
    let exe = executable(agent).ok_or_else(|| {
        io::Error::other(format!(
            "{agent} не найден; установи CLI агента и добавь его в PATH"
        ))
    })?;
    let existing = config(agent, Some(&exe))?;
    let current = std::env::current_exe()?;
    if let Some((command, args)) = &existing {
        if !ours(command, args) {
            return Err(io::Error::other(
                "Имя MCP ferry занято другой конфигурацией. Проверь настройки агента.",
            ));
        }
        if let Some(problem) = mcp_path_error(command, &current) {
            return Err(io::Error::other(problem));
        }
    }
    let managed = backup(agent)?;
    let preserved = skill_state(&skill, &managed)? == "custom";
    if existing.is_none() {
        let executable = current
            .to_str()
            .ok_or_else(|| io::Error::other("путь CLI не UTF-8"))?;
        let args = if agent == "claude" {
            vec![
                "mcp",
                "add",
                "--transport",
                "stdio",
                "--scope",
                "user",
                "ferry",
                "--",
                executable,
                "mcp",
            ]
        } else {
            vec!["mcp", "add", "ferry", "--", executable, "mcp"]
        };
        let (ok, _) = bounded_command(&exe, &args)?;
        if !ok {
            return Err(io::Error::other(
                "CLI агента отклонил добавление MCP; проверь его конфигурацию вручную",
            ));
        }
    }
    let confirmed = config(agent, Some(&exe))?;
    if confirmed.as_ref().is_none_or(|(command, args)| {
        !ours(command, args)
            || Path::new(command).canonicalize().ok() != current.canonicalize().ok()
    }) {
        return Err(io::Error::other(
            "MCP не подтвердился при повторном чтении конфигурации",
        ));
    }
    if !preserved {
        fs::create_dir_all(skill.parent().unwrap())?;
        fs::create_dir_all(managed.parent().unwrap())?;
        ferry_core::write_private(&skill, SKILL)?;
        ferry_core::write_private(&managed, SKILL)?;
    }
    Ok(
        json!({"schema_version":1,"agent":agent,"skill_file":skill,"skill_preserved":preserved,"warning":if preserved{Some("Существующий изменённый скилл сохранён. Новые команды доступны через MCP.")}else{None},"mcp_configured":true,"restart_required":true,"message":"Перезапусти сессию агента, чтобы загрузить инструменты и скилл ferry"}),
    )
}
fn smoke_profile() -> Value {
    let junit = "<testsuite name=\"ferry-smoke\" tests=\"1\" failures=\"0\"><testcase classname=\"connection\" name=\"execution\" time=\"0.01\"/></testsuite>";
    json!({"headless":true,"timeout":30,"junit":"junit.xml","platforms":{
        "macos":{"shell":"bash","command":format!("mkdir -p '{{output}}' && printf '%s' '{}' > '{{output}}/junit.xml' && printf 'ferry smoke passed\\n'",junit)},
        "windows":{"shell":"powershell","command":format!("New-Item -ItemType Directory -Force -Path '{{output}}' | Out-Null; [IO.File]::WriteAllText('{{output}}/junit.xml', '{}'); Write-Output 'ferry smoke passed'",junit)}
    }})
}
fn template(name: &str) -> io::Result<Value> {
    let mut profiles = json!({"smoke":smoke_profile()});
    let command = match name {
        "smoke" => return Ok(json!({"profiles":profiles})),
        "tauri" => "cargo test --workspace",
        "electron" | "web" => "npm test -- --runInBand",
        _ => return Err(io::Error::other("шаблон: smoke, tauri, electron, web")),
    };
    profiles["test"] = json!({"headless":true,"timeout":600,"platforms":{
        "macos":{"shell":"bash","command":command,"tools":[if name=="tauri"{"cargo"}else{"npm"}]},
        "windows":{"shell":"powershell","command":format!("{command}; exit $LASTEXITCODE"),"tools":[if name=="tauri"{"cargo"}else{"npm"}]}
    }});
    Ok(json!({"profiles":profiles}))
}
pub fn init(mut args: &[&str]) -> io::Result<i32> {
    let mut name = "smoke";
    let mut root = std::env::current_dir()?;
    while !args.is_empty() {
        match args {
            ["--template", n, tail @ ..] => {
                name = n;
                args = tail;
            }
            ["--root", p, tail @ ..] => {
                root = PathBuf::from(p);
                args = tail;
            }
            _ => return crate::usage(),
        }
    }
    let contents = template(name)?;
    let file = root.join("ferry.checks.json");
    let mut out = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&file)?;
    out.write_all(serde_json::to_string_pretty(&contents)?.as_bytes())?;
    out.sync_all()?;
    println!(
        "{}",
        json!({"schema_version":1,"file":file,"template":name,"next":"ferry check smoke --devices local","note":"Профиль test — начальная команда проекта; настрой её и JUnit под свой test runner"})
    );
    Ok(0)
}
pub fn run(args: &[&str]) -> io::Result<i32> {
    let value = match args {
        ["status"] => status()?,
        ["install", agent] => install(agent)?,
        ["smoke", device] => {
            let root = ferry_core::ferry_home()?.join("onboarding");
            fs::create_dir_all(&root)?;
            let id = format!("smoke-{}", ferry_core::jobs::unique_id());
            let file = root.join(format!("{id}.json"));
            ferry_core::write_private(
                &file,
                &json!({"profiles":{"smoke":smoke_profile()}}).to_string(),
            )?;
            return crate::checks::run_onboarding(&[
                "smoke",
                "--file",
                &file.to_string_lossy(),
                "--devices",
                device,
                "--home",
                "--id",
                &id,
                "--detach",
            ]);
        }
        _ => return crate::usage(),
    };
    println!("{value}");
    Ok(0)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn skill_updates_only_recognized_contents() {
        assert!(SKILL.starts_with("---\n"));
        assert_eq!(classify_skill(SKILL, None), "current");
        assert_eq!(
            classify_skill(&format!("{MARKER}{SKILL}"), None),
            "upgrade_available"
        );
        assert_eq!(
            classify_skill("saved old text", Some("saved old text")),
            "upgrade_available"
        );
        assert_eq!(
            classify_skill(&format!("{MARKER}{SKILL}custom"), None),
            "custom"
        );
        assert!(
            mcp_path_error("ferry", Path::new("/apps/ferry"))
                .unwrap()
                .contains("PATH")
        );
    }
    #[test]
    fn preserves_foreign_server_and_templates_are_valid_json() {
        assert!(ours(
            "/Applications/ferry.app/Contents/MacOS/ferry",
            &["mcp".into()]
        ));
        assert!(!ours("evil", &["mcp".into()]));
        assert!(!ours("ferry", &["run".into()]));
        for name in ["smoke", "web", "electron", "tauri"] {
            assert!(
                template(name).unwrap()["profiles"]["smoke"]["platforms"]["windows"]["command"]
                    .is_string()
            );
        }
    }
}
