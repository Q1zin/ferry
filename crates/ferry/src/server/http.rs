use super::{Request, execute};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    fs,
    io::{self, Read, Write},
    net::{TcpListener, TcpStream},
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
include!(concat!(env!("OUT_DIR"), "/dashboard_assets.rs"));
const MAX_BODY: usize = 4096;
const MAX_FILE: u64 = 512 * 1024 * 1024;
struct Shared {
    token: String,
    port: u16,
    assets: Option<PathBuf>,
    actions: bool,
    downloads: Mutex<HashMap<String, (PathBuf, Instant)>>,
}
struct Http {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}
fn random() -> io::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| io::Error::other(e.to_string()))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
fn read_request(stream: &mut TcpStream) -> io::Result<Http> {
    let started = Instant::now();
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        if bytes.len() >= 16384 || started.elapsed() > Duration::from_secs(3) {
            return Err(io::Error::other("headers too large"));
        }
        let mut byte = [0];
        stream.read_exact(&mut byte)?;
        bytes.push(byte[0]);
    }
    let text = std::str::from_utf8(&bytes).map_err(io::Error::other)?;
    let mut lines = text.split("\r\n");
    let request: Vec<_> = lines.next().unwrap_or_default().split(' ').collect();
    if request.len() != 3
        || request[2] != "HTTP/1.1"
        || !request[1].starts_with('/')
        || request[1].chars().any(char::is_control)
    {
        return Err(io::Error::other("invalid request"));
    }
    let mut headers = HashMap::new();
    for line in lines.filter(|l| !l.is_empty()) {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| io::Error::other("invalid header"))?;
        if name.is_empty()
            || !name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
            || headers
                .insert(name.to_ascii_lowercase(), value.trim().to_owned())
                .is_some()
        {
            return Err(io::Error::other("duplicate or invalid header"));
        }
    }
    if headers.contains_key("transfer-encoding") {
        return Err(io::Error::other("chunked requests unsupported"));
    }
    let length = headers
        .get("content-length")
        .map(|s| s.parse::<usize>())
        .transpose()
        .map_err(io::Error::other)?
        .unwrap_or(0);
    if length > MAX_BODY {
        return Err(io::Error::other("body too large"));
    }
    let mut body = vec![0; length];
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut offset = 0;
    while offset < length {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| io::Error::other("request body timeout"))?;
        stream.set_read_timeout(Some(remaining))?;
        let read = stream.read(&mut body[offset..])?;
        if read == 0 {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
        offset += read;
    }
    Ok(Http {
        method: request[0].into(),
        path: request[1].into(),
        headers,
        body,
    })
}
fn same_secret(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}
fn authorized(req: &Http, shared: &Shared) -> bool {
    req.headers
        .get("authorization")
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|v| same_secret(v, &shared.token))
}
fn host_allowed(req: &Http, port: u16) -> bool {
    req.headers
        .get("host")
        .is_some_and(|v| v == &format!("127.0.0.1:{port}") || v == &format!("localhost:{port}"))
}
fn origin_allowed(req: &Http, required: bool) -> bool {
    match req.headers.get("origin") {
        Some(origin) => req
            .headers
            .get("host")
            .is_some_and(|h| origin == &format!("http://{h}")),
        None => !required,
    }
}
fn headers(
    stream: &mut TcpStream,
    status: u16,
    mime: &str,
    length: u64,
    attachment: bool,
) -> io::Result<()> {
    let text = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        429 => "Too Many Requests",
        _ => "Internal Server Error",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {text}\r\nContent-Type: {mime}\r\nContent-Length: {length}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nCross-Origin-Resource-Policy: same-origin\r\nContent-Security-Policy: default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; connect-src 'self'; font-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'\r\n{}\r\n",
        if attachment {
            "Content-Disposition: attachment; filename=\"ferry-report.zip\"\r\n"
        } else {
            ""
        }
    )
}
fn respond(stream: &mut TcpStream, status: u16, mime: &str, body: &[u8]) -> io::Result<()> {
    headers(stream, status, mime, body.len() as u64, false)?;
    stream.write_all(body)
}
fn json_response(stream: &mut TcpStream, status: u16, value: Value) -> io::Result<()> {
    let bytes = serde_json::to_vec(&value)?;
    if bytes.len() > 8 * 1024 * 1024 {
        return respond(
            stream,
            400,
            "application/json; charset=utf-8",
            br#"{"error":"response exceeds 8 MiB; use CLI export"}"#,
        );
    }
    respond(stream, status, "application/json; charset=utf-8", &bytes)
}
pub(super) fn read_bounded(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::other("file exceeds size limit"));
    }
    Ok(bytes)
}
pub(super) fn safe_file(root: &Path, relative: &Path) -> io::Result<PathBuf> {
    let mut path = root.to_owned();
    for part in relative.components() {
        let Component::Normal(part) = part else {
            return Err(io::Error::other("invalid path"));
        };
        path.push(part);
        if fs::symlink_metadata(&path)?.file_type().is_symlink() {
            return Err(io::Error::other("symlink refused"));
        }
    }
    if !path.is_file() || !path.canonicalize()?.starts_with(root) {
        return Err(io::Error::other("invalid file"));
    }
    Ok(path)
}
fn static_asset(path: &str, assets: Option<&Path>) -> io::Result<(Vec<u8>, &'static str)> {
    if path.contains(['%', '\\', '?', '#']) || path.split('/').any(|p| p == ".." || p == ".") {
        return Err(io::Error::other("invalid asset"));
    }
    let path = if path == "/" { "/index.html" } else { path };
    let mime = match Path::new(path).extension().and_then(|s| s.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "application/javascript",
        Some("css") => "text/css",
        Some("png") => "image/png",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        _ => return Err(io::Error::other("unsupported asset")),
    };
    let bytes = if let Some(root) = assets {
        let path = safe_file(root, Path::new(path.trim_start_matches('/')))?;
        if fs::metadata(&path)?.len() > 8 * 1024 * 1024 {
            return Err(io::Error::other("asset too large"));
        }
        read_bounded(&path, 8 * 1024 * 1024)?
    } else {
        ASSETS
            .iter()
            .find(|(name, _)| *name == path)
            .ok_or_else(|| io::Error::other("asset not found"))?
            .1
            .to_vec()
    };
    Ok((bytes, mime))
}
fn handle(stream: &mut TcpStream, shared: &Shared) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let req = match read_request(stream) {
        Ok(r) => r,
        Err(_) => return json_response(stream, 400, json!({"error":"некорректный HTTP-запрос"})),
    };
    if !host_allowed(&req, shared.port) || !origin_allowed(&req, req.method == "POST") {
        return json_response(stream, 403, json!({"error":"неверный Host или Origin"}));
    }
    if req.method == "POST" && req.path == "/api" {
        if !authorized(&req, shared) {
            return json_response(
                stream,
                401,
                json!({"error":"Открой ссылку с токеном, которую напечатал ferry server."}),
            );
        }
        if req
            .headers
            .get("content-type")
            .is_none_or(|v| v.split(';').next() != Some("application/json"))
        {
            return json_response(stream, 400, json!({"error":"нужен application/json"}));
        }
        let result = (|| -> io::Result<Value> {
            let request: Request = serde_json::from_slice(&req.body)?;
            let export = request.action == "export";
            let mut value = execute(request, true, shared.actions)?;
            if export && let Some(file) = value["archive_file"].as_str() {
                let root = ferry_core::ferry_home()?
                    .join("dashboard-exports")
                    .canonicalize()?;
                let path = PathBuf::from(file);
                let relative = path.strip_prefix(&root).map_err(io::Error::other)?;
                let path = safe_file(&root, relative)?;
                if fs::metadata(&path)?.len() > MAX_FILE {
                    return Err(io::Error::other(
                        "архив превышает 512 МиБ; используй CLI export",
                    ));
                }
                let ticket = random()?;
                let mut downloads = shared
                    .downloads
                    .lock()
                    .map_err(|_| io::Error::other("download lock"))?;
                downloads.retain(|_, (_, created)| created.elapsed() < Duration::from_secs(3600));
                if downloads.len() >= 32
                    && let Some(oldest) = downloads
                        .iter()
                        .min_by_key(|(_, (_, at))| *at)
                        .map(|(id, _)| id.clone())
                {
                    downloads.remove(&oldest);
                }
                downloads.insert(ticket.clone(), (path, Instant::now()));
                value["download_id"] = json!(ticket);
                value["filename"] = json!("ferry-report.zip");
                value.as_object_mut().unwrap().remove("archive_file");
                value.as_object_mut().unwrap().remove("directory");
                value.as_object_mut().unwrap().remove("report_file");
            }
            Ok(value)
        })();
        return match result {
            Ok(v) => json_response(stream, 200, v),
            Err(e) => json_response(stream, 400, json!({"error":e.to_string()})),
        };
    }
    if req.method == "GET"
        && let Some(ticket) = req.path.strip_prefix("/download/")
    {
        if !authorized(&req, shared) {
            return json_response(stream, 401, json!({"error":"unauthorized"}));
        }
        let path = shared
            .downloads
            .lock()
            .map_err(|_| io::Error::other("download lock"))?
            .get(ticket)
            .filter(|(_, at)| at.elapsed() < Duration::from_secs(3600))
            .map(|(path, _)| path.clone());
        if let Some(path) = path {
            let root = ferry_core::ferry_home()?
                .join("dashboard-exports")
                .canonicalize()?;
            let path = safe_file(&root, path.strip_prefix(&root).map_err(io::Error::other)?)?;
            let file = fs::File::open(path)?;
            let size = file.metadata()?.len();
            if size > MAX_FILE {
                return json_response(stream, 400, json!({"error":"archive too large"}));
            }
            headers(stream, 200, "application/zip", size, true)?;
            io::copy(&mut file.take(size), stream)?;
            return Ok(());
        }
        return json_response(
            stream,
            404,
            json!({"error":"ссылка на архив истекла; экспортируй отчёт ещё раз"}),
        );
    }
    if req.method == "GET"
        && let Ok((bytes, mime)) = static_asset(&req.path, shared.assets.as_deref())
    {
        return respond(stream, 200, mime, &bytes);
    }
    json_response(stream, 404, json!({"error":"not found"}))
}
pub fn serve(port: u16, assets: Option<PathBuf>, actions: bool) -> io::Result<()> {
    let assets = assets
        .map(|p| {
            if fs::symlink_metadata(&p)?.file_type().is_symlink() {
                return Err(io::Error::other(
                    "каталог --assets не должен быть симлинком",
                ));
            }
            p.canonicalize()
        })
        .transpose()?;
    static_asset("/",assets.as_deref()).map_err(|_|io::Error::other("интерфейс не собран: bun run build в apps/ferry, затем cargo build -p ferry; или --assets apps/ferry/dist"))?;
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))?;
    let port = listener.local_addr()?.port();
    let shared = Arc::new(Shared {
        token: random()?,
        port,
        assets,
        actions,
        downloads: Mutex::new(HashMap::new()),
    });
    println!(
        "{}",
        json!({"url":format!("http://127.0.0.1:{port}/#token={}",shared.token),"read_only":!actions,"version":ferry_core::VERSION})
    );
    io::stdout().flush()?;
    let count = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let mut stream = stream?;
        if count.fetch_add(1, Ordering::SeqCst) >= 16 {
            count.fetch_sub(1, Ordering::SeqCst);
            stream.set_write_timeout(Some(Duration::from_secs(1)))?;
            let _ = json_response(&mut stream, 429, json!({"error":"слишком много запросов"}));
            continue;
        }
        let shared = shared.clone();
        let count = count.clone();
        std::thread::spawn(move || {
            let _ = handle(&mut stream, &shared);
            count.fetch_sub(1, Ordering::SeqCst);
        });
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn loopback_server_rejects_cross_origin_and_unauthenticated_requests() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let shared = Shared {
            token: "test-secret".into(),
            port,
            assets: None,
            actions: false,
            downloads: Mutex::new(HashMap::new()),
        };
        let server = std::thread::spawn(move || {
            for _ in 0..5 {
                let (mut stream, _) = listener.accept().unwrap();
                handle(&mut stream, &shared).unwrap();
            }
        });
        for (host, origin, auth, status) in [
            (
                format!("127.0.0.1:{port}"),
                format!("http://127.0.0.1:{port}"),
                "test-secret",
                200,
            ),
            (
                format!("127.0.0.1:{port}"),
                "https://evil.example".into(),
                "test-secret",
                403,
            ),
            (
                format!("evil.example:{port}"),
                format!("http://evil.example:{port}"),
                "test-secret",
                403,
            ),
            (
                format!("127.0.0.1:{port}"),
                format!("http://127.0.0.1:{port}"),
                "bad",
                401,
            ),
            (
                format!("127.0.0.1:{port}"),
                format!("http://127.0.0.1:{port}"),
                "",
                401,
            ),
        ] {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
            let body = r#"{"action":"about"}"#;
            write!(stream,"POST /api HTTP/1.1\r\nHost: {host}\r\nOrigin: {origin}\r\nAuthorization: Bearer {auth}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",body.len()).unwrap();
            let mut reply = String::new();
            stream.read_to_string(&mut reply).unwrap();
            assert!(reply.starts_with(&format!("HTTP/1.1 {status}")), "{reply}");
        }
        server.join().unwrap();
    }
    #[test]
    fn invalid_http_framing_is_rejected_before_body() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let shared = Shared {
            token: "secret".into(),
            port,
            assets: None,
            actions: false,
            downloads: Mutex::new(HashMap::new()),
        };
        let worker = std::thread::spawn(move || {
            for _ in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                handle(&mut stream, &shared).unwrap();
            }
        });
        for extra in [
            "Content-Length: 4097",
            "Host: duplicate",
            "Transfer-Encoding: chunked",
        ] {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
            write!(
                stream,
                "POST /api HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{extra}\r\n\r\n"
            )
            .unwrap();
            let mut reply = String::new();
            stream.read_to_string(&mut reply).unwrap();
            assert!(reply.starts_with("HTTP/1.1 400"));
        }
        worker.join().unwrap();
    }
    #[test]
    fn bounded_files_reject_growth_past_limit() {
        let path = std::env::temp_dir().join(format!(
            "ferry-dashboard-limit-{}",
            ferry_core::jobs::unique_id()
        ));
        fs::write(&path, b"123456789").unwrap();
        assert!(read_bounded(&path, 8).is_err());
        assert_eq!(read_bounded(&path, 9).unwrap(), b"123456789");
        fs::remove_file(path).unwrap();
    }
    #[test]
    fn assets_reject_encoded_paths_and_traversal() {
        for path in [
            "/../index.html",
            "/%2e%2e/index.html",
            "/assets\\key.js",
            "/index.html?token=x",
        ] {
            assert!(static_asset(path, None).is_err());
        }
    }
    #[cfg(unix)]
    #[test]
    fn assets_never_follow_symlinks() {
        let root =
            std::env::temp_dir().join(format!("ferry-assets-{}", ferry_core::jobs::unique_id()));
        fs::create_dir(&root).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", root.join("index.html")).unwrap();
        assert!(safe_file(&root, Path::new("index.html")).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
