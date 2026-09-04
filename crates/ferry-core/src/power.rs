//! Питание Windows-узла: команды для его шелла (Git Bash). Перезагрузка и выключение — через
//! 10 с, их можно отменить: `cancel`.

use std::io;

/// Команда для шелла узла и что сказать человеку, когда она прошла.
pub fn command(action: &str) -> io::Result<(String, &'static str)> {
    let (remote, done) = match action {
        "reboot" => (
            "shutdown //r //t 10 //c 'ferry: перезагрузка по команде ferry'",
            "перезагрузится через 10 с (отменить: ferry power <устройство> cancel)",
        ),
        "shutdown" => (
            "shutdown //s //t 10 //c 'ferry: выключение по команде ferry'",
            "выключится через 10 с (отменить: ferry power <устройство> cancel)",
        ),
        // Без ожидания: связь оборвётся, когда узел уснёт. Разбудить по сети — только по кабелю.
        "sleep" => (
            "powershell -NoProfile -c 'Add-Type -AssemblyName System.Windows.Forms; \
             [System.Windows.Forms.Application]::SetSuspendState([System.Windows.Forms.PowerState]::Suspend, $false, $false)'",
            "засыпает",
        ),
        "cancel" => ("shutdown //a", "перезагрузка или выключение отменены"),
        _ => {
            return Err(io::Error::other(
                "питание: reboot, sleep, shutdown или cancel",
            ));
        }
    };
    // UTF-8 в консоли: иначе ответы shutdown приходят в OEM-кодировке, кракозябрами.
    Ok((format!("chcp.com 65001 >/dev/null 2>&1; {remote}"), done))
}
