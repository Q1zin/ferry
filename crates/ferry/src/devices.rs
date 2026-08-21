//! Устройства — через приложение ferry на этой машине: оно держит связь, CLI его спрашивает.

use std::io;

use ferry_core::control::{self, Request, Response};
use ferry_core::device::{PeerView, Role, Snapshot};

fn ask(request: Request) -> io::Result<Response> {
    match control::call(request)? {
        Response::Error(e) => Err(io::Error::other(e)),
        response => Ok(response),
    }
}

/// `ferry devices`: это устройство и связанные.
pub fn list() -> io::Result<i32> {
    let Response::State(snap) = ask(Request::State)? else {
        return Err(unexpected());
    };
    print!("{}", table(&snap));
    Ok(0)
}

fn table(snap: &Snapshot) -> String {
    let me = &snap.me;
    let mut rows = vec![[
        me.name.clone(),
        "это устройство".into(),
        os(&me.os).into(),
        me.role.label().into(),
        me.mode.label().into(),
    ]];
    for p in &snap.peers {
        rows.push(match &p.status {
            Some(s) => [
                p.name.clone(),
                path(p),
                os(&s.os).into(),
                s.role.label().into(),
                s.mode.label().into(),
            ],
            None => [
                p.name.clone(),
                path(p),
                String::new(),
                String::new(),
                String::new(),
            ],
        });
    }
    let widths: Vec<usize> = (0..5)
        .map(|i| rows.iter().map(|r| r[i].chars().count()).max().unwrap_or(0))
        .collect();
    let mut out = String::new();
    for row in &rows {
        let cells: Vec<String> = row
            .iter()
            .zip(&widths)
            .map(|(cell, w)| format!("{cell}{}", " ".repeat(w - cell.chars().count())))
            .collect();
        out += &format!("  {}\n", cells.join("   ").trim_end());
    }
    if snap.peers.is_empty() {
        out += "\nСвязанных устройств нет: ferry pair — показать код здесь, ferry pair <код> — ввести код с другого.\n";
    }
    out
}

fn path(p: &PeerView) -> String {
    if !p.online {
        return "не на связи".into();
    }
    let how = if p.direct == Some(false) {
        "через relay"
    } else {
        "напрямую"
    };
    match p.rtt_ms {
        Some(ms) => format!("{how}, {ms} мс"),
        None => how.into(),
    }
}

fn os(name: &str) -> &str {
    match name {
        "macos" => "macOS",
        "windows" => "Windows",
        "linux" => "Linux",
        other => other,
    }
}

/// `ferry pair` — показать код; `ferry pair <код|ссылка>` — связать с тем, кто его показал.
pub fn pair(code: Option<&str>) -> io::Result<i32> {
    match code {
        None => {
            let Response::Pairing(info) = ask(Request::OpenPairing)? else {
                return Err(unexpected());
            };
            println!(
                "Код: {}  (работает {} мин)",
                info.code,
                info.seconds_left.div_ceil(60)
            );
            println!(
                "Введи его на другом устройстве: ferry pair {} — или в приложении.",
                info.code
            );
            println!("Из другой сети — ссылка целиком:\n  {}", info.ticket);
        }
        Some(code) => {
            let Response::Peer(peer) = ask(Request::Pair { code: code.into() })? else {
                return Err(unexpected());
            };
            println!("Связано: {} ({})", peer.name, path(&peer));
        }
    }
    Ok(0)
}

/// `ferry role [<устройство>] <роль>`.
pub fn role(device: Option<&str>, role: &str) -> io::Result<i32> {
    let role = Role::parse(role).ok_or_else(|| {
        io::Error::other(format!(
            "роли: both (узел и пульт), node (узел), console (пульт) — а не {role:?}"
        ))
    })?;
    ask(Request::SetRole {
        device: device.map(String::from),
        role,
    })?;
    println!("{}: {}", device.unwrap_or("это устройство"), role.label());
    Ok(0)
}

fn unexpected() -> io::Error {
    io::Error::other("приложение ответило не то — версии ferry и CLI разные?")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferry_core::device::Status;
    use ferry_core::mode::Mode;

    #[test]
    fn table_lines_up_and_explains_offline() {
        let status = |name: &str, os: &str| Status {
            name: name.into(),
            role: Role::Node,
            mode: Mode::Active,
            os: os.into(),
            version: "0.1.0".into(),
        };
        let snap = Snapshot {
            id: "ff".into(),
            me: Status {
                role: Role::Both,
                ..status("macbook", "macos")
            },
            peers: vec![
                PeerView {
                    id: "c6".into(),
                    name: "win-10".into(),
                    online: true,
                    status: Some(status("win-10", "windows")),
                    direct: Some(true),
                    rtt_ms: Some(6),
                },
                PeerView {
                    id: "c7".into(),
                    name: "win-11".into(),
                    online: false,
                    status: None,
                    direct: None,
                    rtt_ms: None,
                },
            ],
            lan: Vec::new(),
            pairing: None,
        };
        let out = table(&snap);
        assert_eq!(
            out,
            "  macbook   это устройство   macOS     узел и пульт   работает\n\
             \x20 win-10    напрямую, 6 мс   Windows   узел           работает\n\
             \x20 win-11    не на связи\n"
        );
    }
}
