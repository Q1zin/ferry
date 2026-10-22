//! Наличие инструментов проверяем в той же оболочке, в которой агент будет работать.
use crate::desk::{self, Shell};
use serde::{Deserialize, Serialize};
use std::{io, process::Stdio, time::Duration};
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub shell: Shell,
    pub tools: Vec<String>,
    pub desktop: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Capabilities {
    pub shell: Shell,
    pub shell_available: bool,
    pub missing_tools: Vec<String>,
    pub locked: Option<bool>,
    pub warnings: Vec<String>,
}
pub fn validate(request: &Request) -> io::Result<()> {
    if request.tools.len() > 32
        || request.tools.iter().any(|t| {
            t.is_empty()
                || t.len() > 80
                || t.starts_with('-')
                || !t
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "._+-".contains(c))
        })
    {
        return Err(io::Error::other(
            "--tool: до 32 имён команд (буквы, цифры, . _ + -), без путей и аргументов",
        ));
    }
    Ok(())
}
pub fn collect(request: Request) -> io::Result<Capabilities> {
    validate(&request)?;
    let mut script = match request.shell {
        Shell::Powershell => "Write-Output 'FERRY_SHELL_READY';\n".into(),
        Shell::Cmd => "echo FERRY_SHELL_READY & ".into(),
        _ => String::from("printf 'FERRY_SHELL_READY\\n'\n"),
    };
    for tool in &request.tools {
        script += &match request.shell {
            Shell::Powershell => format!(
                "if (Get-Command '{tool}' -CommandType Application -ErrorAction SilentlyContinue) {{ Write-Output '{tool}:yes' }} else {{ Write-Output '{tool}:no' }};\n"
            ),
            Shell::Cmd => {
                format!("where {tool} >nul 2>nul && (echo {tool}:yes) || (echo {tool}:no) & ")
            }
            _ => format!(
                "if command -v '{}' >/dev/null 2>&1; then printf '{}:yes\\n'; else printf '{}:no\\n'; fi\n",
                tool, tool, tool
            ),
        };
    }
    script += if request.shell == Shell::Cmd {
        "exit /b 0"
    } else {
        "\nexit 0\n"
    };
    let mut report = Capabilities {
        shell: request.shell,
        shell_available: false,
        missing_tools: request.tools.clone(),
        locked: None,
        warnings: Vec::new(),
    };
    let output = (|| -> io::Result<std::process::Output> {
        let mut command = desk::shell_with(&script, request.shell)?;
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let pid = child.id();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            tx.send(child.wait_with_output()).ok();
        });
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(out) => out,
            Err(_) => {
                desk::kill_tree(pid).ok();
                Err(io::Error::other("оболочка не ответила за 10 с"))
            }
        }
    })();
    match output {
        Ok(out) => {
            let text = String::from_utf8_lossy(&out.stdout);
            report.shell_available =
                out.status.success() && text.lines().any(|l| l.trim() == "FERRY_SHELL_READY");
            report
                .missing_tools
                .retain(|t| !text.lines().any(|l| l.trim() == format!("{t}:yes")));
            if !report.shell_available {
                report
                    .warnings
                    .push("проверка оболочки не завершилась успешно".into());
            }
        }
        Err(e) => report.warnings.push(e.to_string()),
    }
    if request.desktop {
        #[cfg(target_os = "macos")]
        {
            report.locked = Some(crate::screen::locked());
            let (screen, input) = crate::screen::access();
            if !screen {
                report.warnings.push("нет разрешения записи экрана".into());
            }
            if !input {
                report.warnings.push("нет разрешения Accessibility".into());
            }
        }
        #[cfg(windows)]
        {
            match crate::info::collect() {
                Ok(info) => {
                    report.locked = info
                        .iter()
                        .find(|(k, _)| k == "locked")
                        .map(|(_, v)| v.eq_ignore_ascii_case("true") || v == "1")
                }
                Err(e) => report
                    .warnings
                    .push(format!("состояние рабочего стола неизвестно: {e}")),
            }
        }
    }
    Ok(report)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_shell_injection_and_reports_missing_tool() {
        let r = |tool: &str| Request {
            shell: Shell::Default,
            tools: vec![tool.into()],
            desktop: false,
        };
        for t in ["", "git;id", "$(id)", "../git", "-v"] {
            assert!(validate(&r(t)).is_err());
        }
        let c = collect(r("ferry_absent_tool_8462")).unwrap();
        assert!(c.shell_available);
        assert_eq!(c.missing_tools, vec!["ferry_absent_tool_8462"]);
    }
}
