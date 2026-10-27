//! Readiness is tied to a still-running exact job, with a per-launch HTTP marker.
use super::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::Read;
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HttpProbe {
    pub port: u16,
    pub path: String,
    pub status: u16,
    pub contains: String,
}
pub(super) fn execute(
    dir: &Path,
    http: Option<HttpProbe>,
    window: Option<String>,
) -> io::Result<Value> {
    let pid = read(dir, "child").and_then(|s| s.parse::<u32>().ok());
    if !running(dir) {
        return Ok(json!({"ready":false,"running":false,"pid":pid}));
    }
    let mut errors = Vec::new();
    let http_ready = match http {
        Some(p) => match probe_http(&p) {
            Ok(v) => Some(v),
            Err(e) => {
                errors.push(e.to_string());
                Some(false)
            }
        },
        None => None,
    };
    let mut windows = Vec::new();
    let window_ready = match window {
        Some(title) => {
            if title.is_empty() {
                return Err(io::Error::other("probe: нужен заголовок своего окна"));
            }
            if let Some(pid) = pid {
                windows =
                    crate::screen::owned_windows(&crate::control::descendant_pids(pid)?, &title)?;
            }
            Some(!windows.is_empty())
        }
        None => None,
    };
    let alive = running(dir);
    Ok(
        json!({"ready":alive && http_ready.unwrap_or(true) && window_ready.unwrap_or(true),"running":alive,"pid":pid,"http_ready":http_ready,"window_ready":window_ready,"windows":windows,"errors":errors}),
    )
}
fn probe_http(p: &HttpProbe) -> io::Result<bool> {
    if p.port == 0
        || p.path.len() > 2048
        || !p.path.starts_with('/')
        || p.path.chars().any(|c| c.is_control() || c == ' ')
        || !(100..600).contains(&p.status)
        || p.contains.len() < 8
        || p.contains.len() > 1024
    {
        return Err(io::Error::other(
            "HTTP readiness: loopback port, path, status и уникальный маркер body (8–1024 байт) обязательны",
        ));
    }
    let timeout = Duration::from_secs(2);
    let mut socket = std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], p.port)),
        timeout,
    )?;
    socket.set_read_timeout(Some(timeout))?;
    socket.set_write_timeout(Some(timeout))?;
    write!(
        socket,
        "GET {} HTTP/1.0\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
        p.path, p.port
    )?;
    let mut bytes = Vec::new();
    socket.take(65537).read_to_end(&mut bytes)?;
    if bytes.len() > 65536 {
        return Err(io::Error::other("HTTP readiness response больше 64 КиБ"));
    }
    let text = String::from_utf8_lossy(&bytes);
    let Some((headers, body)) = text.split_once("\r\n\r\n") else {
        return Err(io::Error::other("неверный HTTP ответ"));
    };
    let status = headers
        .lines()
        .next()
        .and_then(|s| s.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok());
    Ok(status == Some(p.status) && body.contains(&p.contains))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn http_ready_requires_the_launch_marker() {
        for (body, ready) in [("old-launch", false), ("new-launch", true)] {
            let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let port = listener.local_addr().unwrap().port();
            let server = std::thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut input = Vec::new();
                while !input.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    socket.read_exact(&mut byte).unwrap();
                    input.push(byte[0]);
                    assert!(input.len() <= 4096);
                }
                write!(
                    socket,
                    "HTTP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            });
            assert_eq!(
                probe_http(&HttpProbe {
                    port,
                    path: "/ready".into(),
                    status: 200,
                    contains: "new-launch".into()
                })
                .unwrap(),
                ready
            );
            server.join().unwrap();
        }
        assert!(
            probe_http(&HttpProbe {
                port: 80,
                path: "/\r\nInjected: true".into(),
                status: 200,
                contains: "new-launch".into()
            })
            .is_err()
        );
    }
}
