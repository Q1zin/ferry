//! Бэкенд приложения хаба: команды Tauri — тонкие обёртки над `ferry-core`, та же логика, что
//! у CLI. Команды ходят в узел по ssh и ждут его: `#[tauri::command(async)]` уносит их в пул
//! потоков — без него синхронная команда идёт в главном потоке и окно замирает до ответа.

use base64::Engine;
use ferry_core::{find, hub, info, nodes, relay};
use serde::Serialize;

type Reply<T> = Result<T, String>;

fn err(e: impl ToString) -> String {
    e.to_string()
}

#[derive(Serialize)]
struct NodeRow {
    name: String,
    addr: String,
    user: String,
    paused: bool,
}

/// Узлы из реестра — мгновенно, без ssh: статус каждого пульт спрашивает отдельно (`status`)
/// и показывает по мере ответов.
#[tauri::command(async)]
fn nodes() -> Reply<Vec<NodeRow>> {
    Ok(nodes::list()
        .map_err(err)?
        .into_iter()
        .map(|node| NodeRow {
            name: node.name,
            addr: node.addr,
            user: node.user,
            paused: node.paused,
        })
        .collect())
}

#[derive(Serialize)]
struct StatusRow {
    online: bool,
    summary: String,
    mode: Option<String>,
    desk: bool,
    latency_ms: Option<u32>,
}

/// Статус одного узла: ssh с таймаутом, до ~5 с, если узел пропал.
#[tauri::command(async)]
fn status(host: String) -> StatusRow {
    let status = hub::status(&host);
    StatusRow {
        online: status.online,
        summary: status.summary(),
        mode: status.mode.map(|m| m.label().into()),
        desk: status.desk,
        latency_ms: status.latency_ms,
    }
}

/// Свойства узла: пары «подпись — значение» и предупреждения.
#[derive(Serialize)]
struct Detail {
    properties: Vec<(String, String)>,
    warnings: Vec<String>,
}

#[tauri::command(async)]
fn detail(host: String) -> Reply<Detail> {
    let pairs = hub::info(&host).map_err(err)?;
    Ok(Detail {
        properties: info::describe(&pairs),
        warnings: info::warnings(&pairs),
    })
}

#[derive(Serialize)]
struct JobRow {
    name: String,
    label: String,
    ago: String,
    running: bool,
    cmd: String,
}

#[tauri::command(async)]
fn jobs(host: String) -> Reply<Vec<JobRow>> {
    let jobs = hub::jobs_list(&host).map_err(err)?;
    Ok(jobs
        .into_iter()
        .map(|job| JobRow {
            label: job.label(),
            ago: job.ago(),
            running: job.running(),
            name: job.name,
            cmd: job.cmd,
        })
        .collect())
}

#[tauri::command(async)]
fn job_log(host: String, name: String) -> Reply<String> {
    hub::job_log(&host, &name).map_err(err)
}

#[tauri::command(async)]
fn start_job(host: String, name: String, cmd: String) -> Reply<String> {
    hub::start(&host, &name, true, &cmd).map_err(err)
}

#[tauri::command(async)]
fn kill_job(host: String, name: String) -> Reply<String> {
    hub::kill(&host, &name).map_err(err)
}

/// Снимок экрана узла как data-URL — показать прямо в вебвью.
#[tauri::command(async)]
fn screenshot(host: String) -> Reply<String> {
    let path = hub::screenshot(&host, None).map_err(err)?;
    let bytes = std::fs::read(&path).map_err(err)?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
    Ok(format!("data:image/png;base64,{b64}"))
}

#[tauri::command(async)]
fn input(host: String, args: Vec<String>) -> Reply<String> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    hub::input(&host, &args).map_err(err)
}

#[tauri::command(async)]
fn add_node(name: String, target: String) -> Reply<()> {
    nodes::register(
        &name,
        target.split_once('@').map(|(u, _)| u).unwrap_or_default(),
        target.split_once('@').map(|(_, a)| a).unwrap_or(&target),
    )
    .map_err(err)
}

#[tauri::command(async)]
fn pause(host: String, paused: bool) -> Reply<()> {
    nodes::set_paused(&host, paused).map_err(err)
}

#[tauri::command(async)]
fn rename(host: String, new_name: String) -> Reply<()> {
    nodes::rename(&host, &new_name).map_err(err)
}

#[tauri::command(async)]
fn remove(host: String) -> Reply<()> {
    nodes::remove(&host).map_err(err)
}

/// Найти узел по ключу хоста; вернуть новый адрес, если нашёлся.
#[tauri::command(async)]
fn find(host: String) -> Reply<Option<String>> {
    let addr = nodes::get(&host).map_err(err)?.addr;
    find::find(&host, &addr, true).map_err(err)
}

#[tauri::command(async)]
fn restart(host: String) -> Reply<String> {
    hub::restart(&host).map_err(err)
}

#[tauri::command(async)]
fn power(host: String, action: String) -> Reply<String> {
    hub::power(&host, &action).map_err(err)
}

#[tauri::command(async)]
fn trusted() -> Reply<Vec<String>> {
    relay::trusted().map_err(err)
}

#[tauri::command(async)]
fn trust(host: String) -> Reply<()> {
    relay::trust(Some(&host)).map(|_| ()).map_err(err)
}

#[tauri::command]
fn version() -> String {
    ferry_core::VERSION.into()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            nodes, status, detail, jobs, job_log, start_job, kill_job, screenshot, input, add_node,
            pause, rename, remove, find, restart, power, trusted, trust, version
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// Команды пульта против настоящего узла: `FERRY_LIVE_NODE=<узел> cargo test -p ferry-app`.
/// Без переменной пропускается — CI узла не имеет.
#[cfg(test)]
mod live {
    use super::*;

    #[test]
    fn commands_talk_to_a_real_node() {
        let Ok(host) = std::env::var("FERRY_LIVE_NODE") else {
            return;
        };
        assert!(
            nodes().unwrap().iter().any(|n| n.name == host),
            "узла нет в реестре"
        );
        let row = status(host.clone());
        assert!(row.online, "{host} не на связи: {}", row.summary);

        let detail = detail(host.clone()).unwrap();
        assert!(
            detail
                .properties
                .iter()
                .any(|(label, _)| label == "система"),
            "{:?}",
            detail.properties
        );

        let shot = screenshot(host.clone()).unwrap();
        assert!(
            shot.starts_with("data:image/png;base64,iVBOR"),
            "снимок не PNG"
        );

        start_job(
            host.clone(),
            "hub-live".into(),
            "echo hello-from-hub; sleep 20".into(),
        )
        .unwrap();
        let listed = jobs(host.clone()).unwrap();
        assert!(
            listed.iter().any(|j| j.name == "hub-live" && j.running),
            "задачи нет в списке"
        );
        std::thread::sleep(std::time::Duration::from_secs(2));
        assert!(
            job_log(host.clone(), "hub-live".into())
                .unwrap()
                .contains("hello-from-hub")
        );
        assert!(
            kill_job(host.clone(), "hub-live".into())
                .unwrap()
                .contains("остановлена")
        );

        assert_eq!(version(), ferry_core::VERSION);
    }
}
