//! Поиск узла, сменившего адрес. Узел узнаётся по ключу хоста, а не по IP.
//!
//! Host-блок узла несёт `HostKeyAlias <узел>`: ssh помнит ключ под именем узла, и смена адреса
//! не ломает проверку ключа. Если узел не отвечает, `find` обходит подсети хаба (/24 вокруг
//! каждого его адреса и прежнего адреса узла): у кого открыт 22-й порт и чей ключ совпал с
//! известным — тот и есть узел, его адрес уходит в `HostName`. Чужая машина с открытым 22-м
//! портом за узел себя не выдаст: ключ не тот.

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use crate::{ferry_home, home, write_private};

/// Найти узел и записать его новый адрес. `Ok(Some(адрес))` — нашёлся (или и не терялся).
pub fn find(name: &str, addr: &str, quiet: bool) -> io::Result<Option<String>> {
    let known = known_keys(&[name, addr]);
    if known.is_empty() {
        return Err(io::Error::other(format!(
            "ключ {name} неизвестен — узнать его не по чему; ferry add {name} <user@адрес>"
        )));
    }
    if keys_at(addr).iter().any(|k| known.contains(k)) {
        remember(name, addr, &known)?;
        return Ok(Some(addr.to_string()));
    }

    // Сначала tailnet: адрес устройства там не зависит от того, в какой оно сети.
    let peers: Vec<String> = tailscale_peers()
        .into_iter()
        .filter(|ip| ip != addr)
        .collect();
    if let Some(ip) = first_with_key(&peers, &known) {
        remember(name, &ip, &known)?;
        set_hostname(name, &ip)?;
        eprintln!("ferry: {name} нашёлся в tailnet на {ip} (был {addr})");
        return Ok(Some(ip));
    }

    let subnets = lan_subnets();
    if !quiet {
        let list: Vec<String> = subnets.iter().map(|s| format!("{s}.0/24")).collect();
        eprintln!(
            "ferry: ищу {name} в локальных сетях хаба: {} …",
            list.join(", ")
        );
    }
    let open = scan(&subnets);
    if let Some(ip) = first_with_key(&open, &known) {
        remember(name, &ip, &known)?;
        set_hostname(name, &ip)?;
        eprintln!("ferry: {name} нашёлся на {ip} (был {addr})");
        return Ok(Some(ip));
    }
    if !quiet {
        eprintln!("ferry: {name} в этих подсетях нет.");
        if open.is_empty() {
            eprintln!("  открытого ssh (порт 22) ни у кого нет");
        } else {
            eprintln!("  ssh открыт у {}, но ключ не тот", open.join(", "));
        }
        eprintln!(
            "  Узел включён и в той же сети? Если он в другой подсети (свой роутер, статический \
             IP), хаб туда не дотянется — скажи адрес: ferry add {name} <user@адрес>"
        );
    }
    Ok(None)
}

/// Ключи хоста, которые знает ssh, по любому из имён: `тип base64`.
fn known_keys(names: &[&str]) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    for name in names {
        let Ok(out) = Command::new("ssh-keygen").args(["-F", name]).output() else {
            continue;
        };
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            if let [_, kind, key, ..] = line.split_whitespace().collect::<Vec<_>>()[..] {
                if !line.starts_with('#') {
                    keys.insert(format!("{kind} {key}"));
                }
            }
        }
    }
    keys
}

/// Ключи, которые предъявляет sshd по адресу: `тип base64`.
fn keys_at(ip: &str) -> BTreeSet<String> {
    let out = Command::new("ssh-keyscan")
        .args(["-T", "3", ip])
        .stderr(Stdio::null())
        .output();
    let Ok(out) = out else {
        return BTreeSet::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(
            |line| match line.split_whitespace().collect::<Vec<_>>()[..] {
                [_, kind, key, ..] => Some(format!("{kind} {key}")),
                _ => None,
            },
        )
        .collect()
}

/// Адреса узлов tailnet, которые сейчас на связи, — если на хабе есть Tailscale.
fn tailscale_peers() -> Vec<String> {
    for cli in [
        "tailscale",
        "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
    ] {
        let Ok(out) = Command::new(cli)
            .arg("status")
            .stderr(Stdio::null())
            .output()
        else {
            continue;
        };
        if !out.status.success() {
            continue;
        }
        // Строки `адрес имя пользователь ОС состояние`; первая — сам хаб, у него sshd нет.
        return String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|line| !line.contains("offline"))
            .filter_map(|line| line.split_whitespace().next())
            .filter(|ip| ip.parse::<Ipv4Addr>().is_ok())
            .map(String::from)
            .collect();
    }
    Vec::new()
}

/// Первые три октета локальных сетей хаба — интерфейсов с широковещательным адресом.
/// Туннели (VPN, Tailscale) — точка-точка, их не трогаем: сканировать чужую сеть нельзя,
/// а VPN вдобавок может принимать соединение на любой адрес.
// ponytail: только /24 вокруг своего адреса; сеть /16 целиком так не обойти — понадобится,
// возьмём маску интерфейса.
fn lan_subnets() -> Vec<String> {
    let Ok(out) = Command::new("ifconfig").stderr(Stdio::null()).output() else {
        return Vec::new();
    };
    lan_prefixes(&String::from_utf8_lossy(&out.stdout))
}

/// `inet 192.168.100.17 netmask 0xffffff00 broadcast 192.168.100.255` → `192.168.100`.
fn lan_prefixes(ifconfig: &str) -> Vec<String> {
    let mut found = BTreeSet::new();
    for line in ifconfig.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        if words.first() != Some(&"inet") || !words.contains(&"broadcast") {
            continue;
        }
        let Ok(ip) = words[1].parse::<Ipv4Addr>() else {
            continue;
        };
        if ip.is_loopback() || ip.is_link_local() {
            continue;
        }
        let [a, b, c, _] = ip.octets();
        found.insert(format!("{a}.{b}.{c}"));
    }
    found.into_iter().collect()
}

/// Первый адрес, где sshd предъявляет известный ключ. Спрашиваем всех сразу и не больше
/// 32: если «открыт» каждый второй адрес, это не сеть, а что-то, что отвечает за всех.
fn first_with_key(ips: &[String], known: &BTreeSet<String>) -> Option<String> {
    thread::scope(|s| {
        let checks: Vec<_> = ips
            .iter()
            .take(32)
            .map(|ip| {
                s.spawn(move || {
                    keys_at(ip)
                        .iter()
                        .any(|k| known.contains(k))
                        .then(|| ip.clone())
                })
            })
            .collect();
        checks.into_iter().find_map(|c| c.join().ok().flatten())
    })
}

/// У кого в подсетях открыт 22-й порт.
fn scan(subnets: &[String]) -> Vec<String> {
    let candidates: Vec<String> = subnets
        .iter()
        .flat_map(|net| (1..255).map(move |i| format!("{net}.{i}")))
        .collect();
    let mut open: Vec<String> = thread::scope(|s| {
        let probes: Vec<_> = candidates
            .iter()
            .map(|ip| {
                s.spawn(move || {
                    let addr: SocketAddr = format!("{ip}:22").parse().ok()?;
                    TcpStream::connect_timeout(&addr, Duration::from_millis(700))
                        .ok()
                        .map(|_| ip.clone())
                })
            })
            .collect();
        probes
            .into_iter()
            .filter_map(|p| p.join().ok().flatten())
            .collect()
    });
    open.sort_by_key(|ip| ip.parse::<Ipv4Addr>().map(u32::from).unwrap_or(0));
    open
}

/// Ключ узла — под его именем в known_hosts: дальше проверка не зависит от адреса.
fn remember(name: &str, ip: &str, known: &BTreeSet<String>) -> io::Result<()> {
    if !known_keys(&[name]).is_empty() {
        return Ok(());
    }
    // Только те, что узел предъявил сейчас и которые мы уже знали.
    let keys: Vec<String> = keys_at(ip).intersection(known).cloned().collect();
    let path = home()?.join(".ssh/known_hosts");
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    for key in keys {
        writeln!(file, "{name} {key}")?;
    }
    Ok(())
}

/// `HostName` узла в `~/.ferry/ssh_config` — на новый адрес; `HostKeyAlias` — если не было.
pub fn set_hostname(name: &str, ip: &str) -> io::Result<()> {
    let path = ferry_home()?.join("ssh_config");
    let text = fs::read_to_string(&path)?;
    write_private(&path, &with_hostname(&text, name, ip))?;
    // Общее соединение, если вдруг живо, — к старому адресу.
    let _ = Command::new("ssh")
        .args(["-O", "exit", name])
        .stderr(Stdio::null())
        .status();
    Ok(())
}

/// Новый `HostName` в блоке узла и `HostKeyAlias` сразу за ним (прежний, где бы ни стоял,
/// убирается — так он ровно один).
fn with_hostname(text: &str, name: &str, ip: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for line in text.lines() {
        if let Some(host) = line.strip_prefix("Host ") {
            inside = host.trim() == name;
        }
        let key = line.trim_start();
        if inside && key.starts_with("HostKeyAlias ") {
            continue;
        }
        if inside && key.starts_with("HostName ") {
            out.push_str(&format!("  HostName {ip}\n  HostKeyAlias {name}\n"));
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_only_broadcast_lans_not_tunnels() {
        let ifconfig = "lo0: flags=8049<UP,LOOPBACK,RUNNING,MULTICAST> mtu 16384
\tinet 127.0.0.1 netmask 0xff000000
en0: flags=8863<UP,BROADCAST,SMART,RUNNING,SIMPLEX,MULTICAST> mtu 1500
\tinet 192.168.100.17 netmask 0xffffff00 broadcast 192.168.100.255
bridge100: flags=8863<UP,BROADCAST> mtu 1500
\tinet 169.254.10.1 netmask 0xffff0000 broadcast 169.254.255.255
utun5: flags=8051<UP,POINTOPOINT,RUNNING,MULTICAST> mtu 1500
\tinet 172.16.0.1 --> 172.16.0.1 netmask 0xfffffffc
utun10: flags=8051<UP,POINTOPOINT,RUNNING,MULTICAST> mtu 1280
\tinet 100.75.204.87 --> 100.75.204.87 netmask 0xffffffff
";
        assert_eq!(lan_prefixes(ifconfig), ["192.168.100"]);
    }

    #[test]
    fn moves_only_the_named_host_and_keeps_one_alias() {
        let text = "Host a\n  HostName 1.1.1.1\n  User u\n\nHost b\n  HostName 2.2.2.2\n";
        let moved = with_hostname(text, "a", "9.9.9.9");
        assert_eq!(
            moved,
            "Host a\n  HostName 9.9.9.9\n  HostKeyAlias a\n  User u\n\nHost b\n  HostName 2.2.2.2\n"
        );
        assert_eq!(
            with_hostname(&moved, "a", "8.8.8.8"),
            moved.replace("9.9.9.9", "8.8.8.8")
        );
        // Блок, где алиас стоял в конце (так писала первая версия), чинится.
        let tail = "Host a\n  HostName 1.1.1.1\n  User u\n\n  HostKeyAlias a\n";
        assert_eq!(
            with_hostname(tail, "a", "9.9.9.9"),
            "Host a\n  HostName 9.9.9.9\n  HostKeyAlias a\n  User u\n\n"
        );
    }
}
