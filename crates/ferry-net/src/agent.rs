//! Агент устройства: endpoint iroh, связь со всеми связанными устройствами и связывание
//! новых. С каждым связанным устройством держит соединение и подписку на его статус —
//! изменения приходят сами, обрыв виден сразу. Приложение и CLI работают через агента.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use ferry_core::device::{PairingInfo, PeerView, Snapshot};
use ferry_core::mode::{self, Mode};
use iroh::address_lookup::MemoryLookup;
use iroh::dns::{DnsResolver, NameserverConfig};
use iroh::endpoint::{Connection, QuicTransportConfig, presets};
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode};
use irpc::WithChannels;
use tokio::sync::watch;
use tokio::task::AbortHandle;

use crate::device::Device;
use crate::lan::Lan;
use crate::node::Node;
use crate::pair;
use crate::proto::Fail;
use crate::proto::{
    self, DeviceClient, DeviceMessage, DeviceProtocol, Rename, Role, SetRole, Status,
};
use crate::store::{self, Peer, Store};

pub struct Options {
    /// Где ключ, `device.json` и файл режима: `~/.ferry`.
    pub dir: PathBuf,
    /// Relay и поиск по ключу через интернет (n0). Без него — только адреса, которые агент
    /// узнал сам: из локальной сети или через [`Agent::learn`].
    pub internet: bool,
    /// Поиск в локальной сети (mDNS).
    pub lan: bool,
    /// Дом пользователя: проекты на этом устройстве — в `<home>/ferry/<имя>`.
    pub home: PathBuf,
    /// Бинарник с `job-supervise <имя>` — надсмотрщик фоновых задач: само приложение или `ferry`.
    pub supervisor: PathBuf,
}

impl Options {
    pub fn new(dir: PathBuf) -> Options {
        Options {
            dir,
            internet: true,
            lan: true,
            home: ferry_core::home().unwrap_or_default(),
            supervisor: std::env::current_exe().unwrap_or_default(),
        }
    }
}

#[derive(Clone)]
pub struct Agent {
    inner: Arc<Inner>,
    _router: Arc<Router>,
}

struct Inner {
    dir: PathBuf,
    endpoint: Endpoint,
    lookup: MemoryLookup,
    store: Mutex<Store>,
    me: watch::Sender<Status>,
    peers: Mutex<BTreeMap<EndpointId, Live>>,
    lan: Option<Lan>,
    pairing: Mutex<Option<pair::Window>>,
    changed: Arc<watch::Sender<u64>>,
    /// Команды, которые это устройство выполняет для других.
    node: Arc<Node>,
    /// Открытые туннели: (устройство, порт).
    tunnels: Mutex<BTreeMap<(String, u16), AbortHandle>>,
}

/// Что известно о связанном устройстве прямо сейчас.
#[derive(Default)]
struct Live {
    /// Есть, пока соединение живо и подписка на статус работает.
    client: Option<DeviceClient>,
    status: Option<Status>,
    direct: Option<bool>,
    rtt_ms: Option<u32>,
    task: Option<AbortHandle>,
}

/// DNS — через HTTPS (Cloudflare, Google). VPN на любой из сторон бывает, и с ним обычный DNS
/// (UDP и TCP на 53 порт) отвечает ошибкой, которую резолвер считает окончательной: обычные
/// серверы в основном списке всё ломают. Закрыт HTTPS к ним — резолвер сам перейдёт к запасным
/// публичным серверам.
// ponytail: сеть, где наружу закрыт любой DNS, кроме своего, так не заработает — понадобится,
// добавим системный резолвер запасным ярусом.
fn resolver() -> DnsResolver {
    DnsResolver::builder()
        .add_nameserver_configs(
            ["1.1.1.1", "8.8.8.8"].map(|ip| NameserverConfig::https(ip.parse().unwrap())),
        )
        .build()
}

impl Agent {
    pub async fn start(opts: Options) -> Result<Agent> {
        let key = store::key(&opts.dir)?;
        let store = Store::load(&opts.dir)?;
        let lookup = MemoryLookup::new();
        let builder = if opts.internet {
            Endpoint::builder(presets::N0).dns_resolver(resolver())
        } else {
            Endpoint::builder(presets::Minimal).relay_mode(RelayMode::Disabled)
        };
        // Пропавшее устройство (выключили, ушло из сети) видно через 12 с, а не через 30 по
        // умолчанию: iroh шлёт heartbeat раз в 5 с, один пропуск переживём.
        let transport = QuicTransportConfig::builder()
            .max_idle_timeout(Some(Duration::from_secs(12).try_into()?))
            .build();
        let endpoint = builder
            .transport_config(transport)
            .secret_key(key)
            .alpns(vec![proto::ALPN.to_vec(), pair::ALPN.to_vec()])
            .address_lookup(lookup.clone())
            .bind()
            .await?;

        let changed = Arc::new(watch::channel(0u64).0);
        let lan = if opts.lan {
            let notify = changed.clone();
            let port = endpoint
                .bound_sockets()
                .into_iter()
                .find(|a| a.is_ipv4())
                .map_or(0, |a| a.port());
            Lan::start(endpoint.id(), port, lookup.clone(), move || {
                notify.send_modify(|n| *n += 1)
            })
            .inspect_err(|e| tracing::warn!("поиск в локальной сети не запустился: {e:#}"))
            .ok()
        } else {
            None
        };
        let me = Status {
            name: store.name.clone(),
            role: store.role,
            mode: mode::current_in(&opts.dir),
            os: std::env::consts::OS.into(),
            version: ferry_core::VERSION.into(),
        };
        let peers: Vec<EndpointId> = store.peers.iter().map(|p| p.id).collect();
        let me = watch::channel(me).0;
        let node = Arc::new(Node {
            dir: opts.dir.clone(),
            home: opts.home,
            supervisor: opts.supervisor,
            me: me.subscribe(),
            runs: Mutex::default(),
        });
        let inner = Arc::new(Inner {
            dir: opts.dir,
            endpoint: endpoint.clone(),
            lookup,
            store: Mutex::new(store),
            me,
            node,
            tunnels: Mutex::default(),
            peers: Mutex::default(),
            lan,
            pairing: Mutex::default(),
            changed,
        });
        let router = Router::builder(endpoint)
            .accept(proto::ALPN, DeviceHandler(inner.clone()))
            .accept(pair::ALPN, PairHandler(inner.clone()))
            .spawn();
        inner.announce();
        for id in peers {
            inner.clone().keep(id);
        }
        tokio::spawn(inner.clone().background());
        Ok(Agent {
            inner,
            _router: Arc::new(router),
        })
    }

    pub fn id(&self) -> EndpointId {
        self.inner.endpoint.id()
    }

    /// Адрес этого устройства: ключ и сетевые адреса.
    pub fn addr(&self) -> EndpointAddr {
        self.inner.endpoint.addr()
    }

    /// Запомнить адрес устройства, которое агент сам бы не нашёл (тесты, ручной адрес).
    pub fn learn(&self, addr: EndpointAddr) {
        self.inner.lookup.add_endpoint_info(addr);
    }

    /// Сообщает о каждом изменении: тогда стоит взять свежий [`Agent::snapshot`].
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.inner.changed.subscribe()
    }

    pub fn snapshot(&self) -> Snapshot {
        let inner = &self.inner;
        let store = inner.store.lock().unwrap().clone();
        let live = inner.peers.lock().unwrap();
        let peers = store
            .peers
            .iter()
            .map(|peer| {
                let l = live.get(&peer.id);
                PeerView {
                    id: peer.id.to_string(),
                    name: l
                        .and_then(|l| l.status.as_ref())
                        .map_or(peer.name.clone(), |s| s.name.clone()),
                    online: l.is_some_and(|l| l.client.is_some()),
                    status: l.and_then(|l| l.status.clone()),
                    direct: l.and_then(|l| l.direct),
                    rtt_ms: l.and_then(|l| l.rtt_ms),
                }
            })
            .collect();
        let lan = inner.lan.as_ref().map_or_else(Vec::new, |lan| {
            lan.devices()
                .into_iter()
                .filter(|d| store.peers.iter().all(|p| p.id.to_string() != d.id))
                .collect()
        });
        Snapshot {
            id: self.id().to_string(),
            me: inner.me.borrow().clone(),
            peers,
            lan,
            pairing: inner.pairing_info(),
        }
    }

    /// Показать код: это устройство ждёт, что его добавят с другого.
    pub fn open_pairing(&self) -> PairingInfo {
        *self.inner.pairing.lock().unwrap() = Some(pair::Window::open());
        self.inner.announce();
        self.inner.notify();
        self.inner.pairing_info().expect("окно только что открыто")
    }

    pub fn close_pairing(&self) {
        *self.inner.pairing.lock().unwrap() = None;
        self.inner.announce();
        self.inner.notify();
    }

    /// Ввести код или ссылку с другого устройства. В одной сети устройство находится само:
    /// пробуем все, что ждут связывания, — подойдёт только то, что показало этот код.
    pub async fn pair(&self, input: &str) -> Result<PeerView> {
        let inner = &self.inner;
        let (target, code) = pair::parse(input)?;
        let candidates: Vec<EndpointId> = match target {
            Some(id) => vec![id],
            None => self
                .snapshot()
                .lan
                .into_iter()
                .filter(|d| d.pairing)
                .filter_map(|d| d.id.parse().ok())
                .collect(),
        };
        if candidates.is_empty() {
            bail!(
                "рядом нет устройства, которое ждёт связывания: открой «Связать устройство» на нём, \
                 а если оно в другой сети — вставь ссылку целиком"
            );
        }
        if candidates.contains(&self.id()) {
            bail!("это ссылка на само это устройство");
        }
        let name = inner.store.lock().unwrap().name.clone();
        let mut attempts = tokio::task::JoinSet::new();
        for id in candidates {
            let (endpoint, name, code) = (inner.endpoint.clone(), name.clone(), code.clone());
            attempts.spawn(async move {
                let conn =
                    tokio::time::timeout(Duration::from_secs(20), endpoint.connect(id, pair::ALPN))
                        .await
                        .context("устройство не ответило за 20 с")??;
                let peer_name = pair::initiate(&conn, endpoint.id(), &name, &code).await?;
                conn.close(0u32.into(), b"paired");
                anyhow::Ok((id, peer_name))
            });
        }
        let mut last = None;
        while let Some(result) = attempts.join_next().await {
            match result? {
                Ok((id, peer_name)) => {
                    inner.clone().add_peer(id, peer_name);
                    return Ok(self
                        .snapshot()
                        .peers
                        .into_iter()
                        .find(|p| p.id == id.to_string())
                        .unwrap());
                }
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap())
    }

    /// Роль этого устройства (`None`) или связанного.
    pub async fn set_role(&self, target: Option<EndpointId>, role: Role) -> Result<()> {
        match target {
            None => self.inner.set_role(role).map_err(anyhow::Error::msg),
            Some(id) => self.inner.client(id)?.set_role(role).await,
        }
    }

    pub async fn rename(&self, target: Option<EndpointId>, name: &str) -> Result<()> {
        let name = store::valid_name(name).map_err(anyhow::Error::msg)?;
        match target {
            None => self.inner.rename(name).map_err(anyhow::Error::msg),
            Some(id) => self.inner.client(id)?.rename(name).await,
        }
    }

    /// Режим этого устройства. Чужой режим не меняют: его выбирает человек за устройством.
    pub fn set_mode(&self, mode: Mode) -> Result<()> {
        mode::set_in(&self.inner.dir, mode)?;
        self.inner.update_me(|s| s.mode = mode);
        Ok(())
    }

    /// Забыть связанное устройство.
    pub fn forget(&self, id: EndpointId) -> Result<()> {
        let inner = &self.inner;
        {
            let mut store = inner.store.lock().unwrap();
            store.peers.retain(|p| p.id != id);
            store.save(&inner.dir)?;
        }
        if let Some(task) = inner.peers.lock().unwrap().remove(&id).and_then(|l| l.task) {
            task.abort();
        }
        inner.notify();
        Ok(())
    }

    /// Связанное устройство на связи — по имени или началу ключа.
    pub fn device(&self, who: &str) -> Result<Device, Fail> {
        let snapshot = self.snapshot();
        let peer = snapshot.find(who).map_err(Fail::new)?;
        let id: EndpointId = peer.id.parse().map_err(Fail::new)?;
        let client = self
            .inner
            .client(id)
            .map_err(|e| Fail::new(format!("{e:#}")))?;
        Ok(Device {
            id,
            name: peer.name.clone(),
            client,
            dir: self.inner.dir.clone(),
        })
    }

    /// `127.0.0.1:<port>` здесь → тот же порт устройства (CDP, dev-сервер). Порт тот же с обеих
    /// сторон: CDP отдаёт в `/json/version` ws-адрес со своим портом.
    pub async fn open_tunnel(&self, who: &str, port: u16) -> Result<String, Fail> {
        let device = self.device(who)?;
        let key = (device.name.clone(), port);
        if self.inner.tunnels.lock().unwrap().contains_key(&key) {
            return Ok(format!(
                "ferry: туннель уже открыт: 127.0.0.1:{port} → {}:{port}",
                device.name
            ));
        }
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .map_err(|_| {
                Fail::new(format!(
                    "порт {port} здесь занят — запусти на устройстве на другом порту"
                ))
            })?;
        let agent = self.clone();
        let name = device.name.clone();
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                // Устройство могло переподключиться — клиент берём свежий на каждое соединение.
                let Ok(device) = agent.device(&name) else {
                    continue;
                };
                tokio::spawn(async move {
                    if let Ok((tx, rx)) = device.tunnel(port).await {
                        crate::node::pipe(stream, tx, rx).await;
                    }
                });
            }
        });
        self.inner
            .tunnels
            .lock()
            .unwrap()
            .insert(key, task.abort_handle());
        Ok(format!("ferry: 127.0.0.1:{port} → {}:{port}", device.name))
    }

    pub fn close_tunnel(&self, who: &str, port: u16) -> Result<String, Fail> {
        let mut tunnels = self.inner.tunnels.lock().unwrap();
        let key = tunnels
            .keys()
            .find(|(name, p)| *p == port && (name == who || who.is_empty()))
            .cloned();
        match key.and_then(|key| tunnels.remove(&key).map(|task| (key, task))) {
            Some(((name, port), task)) => {
                task.abort();
                Ok(format!("ferry: туннель {name}:{port} закрыт"))
            }
            None => Err(Fail::new(format!("туннеля {who}:{port} нет"))),
        }
    }

    /// Открытые туннели: устройство и порт.
    pub fn tunnels(&self) -> Vec<(String, u16)> {
        self.inner.tunnels.lock().unwrap().keys().cloned().collect()
    }

    pub async fn shutdown(&self) {
        self.inner.endpoint.close().await;
    }
}

impl Inner {
    fn notify(&self) {
        self.changed.send_modify(|n| *n += 1);
    }

    fn update_me(&self, change: impl FnOnce(&mut Status)) {
        let modified = self.me.send_if_modified(|status| {
            let before = status.clone();
            change(status);
            *status != before
        });
        if modified {
            self.notify();
        }
    }

    fn announce(&self) {
        let Some(lan) = &self.lan else { return };
        let name = self.store.lock().unwrap().name.clone();
        let pairing = self.pairing_info().is_some();
        if let Err(e) = lan.announce(&name, pairing) {
            tracing::warn!("объявить себя в локальной сети не вышло: {e:#}");
        }
    }

    fn pairing_info(&self) -> Option<PairingInfo> {
        let mut window = self.pairing.lock().unwrap();
        let left = window.as_ref()?.left();
        if left.is_zero() {
            *window = None;
            return None;
        }
        let code = window.as_ref()?.code.clone();
        Some(PairingInfo {
            ticket: pair::ticket(self.endpoint.id(), &code),
            code,
            seconds_left: left.as_secs(),
        })
    }

    fn set_role(&self, role: Role) -> Result<(), String> {
        let mut store = self.store.lock().unwrap();
        store.role = role;
        store.save(&self.dir).map_err(|e| e.to_string())?;
        drop(store);
        self.update_me(|s| s.role = role);
        Ok(())
    }

    fn rename(&self, name: String) -> Result<(), String> {
        let mut store = self.store.lock().unwrap();
        store.name = name.clone();
        store.save(&self.dir).map_err(|e| e.to_string())?;
        drop(store);
        self.update_me(|s| s.name = name);
        self.announce();
        Ok(())
    }

    /// Менять роль и имя с другого устройства можно, пока человек за этим не отключил его.
    fn remote_change(&self, change: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
        let me = self.me.borrow().clone();
        if me.mode == Mode::Paused {
            return Err(format!(
                "{} отключён от других устройств — это выбрали на самом устройстве (иконка ferry в трее)",
                me.name
            ));
        }
        change()
    }

    fn client(&self, id: EndpointId) -> Result<DeviceClient> {
        let name = self.store.lock().unwrap().peer(id).map(|p| p.name.clone());
        let Some(name) = name else {
            bail!("устройство {} не связано с этим", id.fmt_short());
        };
        self.peers
            .lock()
            .unwrap()
            .get(&id)
            .and_then(|l| l.client.clone())
            .with_context(|| format!("{name} не на связи"))
    }

    fn add_peer(self: Arc<Self>, id: EndpointId, name: String) {
        {
            let mut store = self.store.lock().unwrap();
            match store.peers.iter_mut().find(|p| p.id == id) {
                Some(peer) => peer.name = name,
                None => store.peers.push(Peer { id, name }),
            }
            if let Err(e) = store.save(&self.dir) {
                tracing::warn!("не сохранить связанное устройство: {e:#}");
            }
        }
        self.clone().keep(id);
        self.notify();
    }

    /// Держать связь с устройством: подключиться, подписаться на статус, при обрыве — снова.
    fn keep(self: Arc<Self>, id: EndpointId) {
        let mut peers = self.peers.lock().unwrap();
        let live = peers.entry(id).or_default();
        if live.task.is_some() {
            return;
        }
        let inner = self.clone();
        let task = tokio::spawn(async move {
            let mut pause = Duration::from_secs(1);
            loop {
                match inner.watch_peer(id).await {
                    Ok(()) => pause = Duration::from_secs(1),
                    Err(e) => tracing::debug!("{}: {e:#}", id.fmt_short()),
                }
                inner.lost(id);
                tokio::time::sleep(pause).await;
                pause = (pause * 2).min(Duration::from_secs(15));
            }
        });
        live.task = Some(task.abort_handle());
    }

    async fn watch_peer(&self, id: EndpointId) -> Result<()> {
        let conn = tokio::time::timeout(
            Duration::from_secs(20),
            self.endpoint.connect(id, proto::ALPN),
        )
        .await
        .context("не ответило за 20 с")??;
        let client = DeviceClient::new(conn.clone());
        let mut statuses = client.watch().await?;
        let mut client = Some(client);
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        loop {
            tokio::select! {
                next = statuses.recv() => match next? {
                    Some(status) => self.on_status(id, status, client.take(), &conn),
                    None => return Ok(()),
                },
                _ = tick.tick() => self.on_path(id, &conn),
            }
        }
    }

    fn on_status(
        &self,
        id: EndpointId,
        status: Status,
        client: Option<DeviceClient>,
        conn: &Connection,
    ) {
        {
            let mut store = self.store.lock().unwrap();
            if let Some(peer) = store
                .peers
                .iter_mut()
                .find(|p| p.id == id && p.name != status.name)
            {
                peer.name = status.name.clone();
                let _ = store.save(&self.dir);
            }
        }
        if let Some(live) = self.peers.lock().unwrap().get_mut(&id) {
            live.status = Some(status);
            if client.is_some() {
                live.client = client;
            }
        }
        self.on_path(id, conn);
        self.notify();
    }

    fn on_path(&self, id: EndpointId, conn: &Connection) {
        let paths = conn.paths();
        let Some(path) = paths.into_iter().find(|p| p.is_selected()) else {
            return;
        };
        let (direct, rtt) = (Some(path.is_ip()), Some(path.rtt().as_millis() as u32));
        let mut peers = self.peers.lock().unwrap();
        let Some(live) = peers.get_mut(&id) else {
            return;
        };
        // Задержка гуляет на миллисекунды — шуметь событиями из-за этого незачем.
        let jumped = live
            .rtt_ms
            .zip(rtt)
            .is_none_or(|(a, b)| a.abs_diff(b) > a.max(b) / 4 + 2);
        if live.direct != direct || jumped {
            live.direct = direct;
            live.rtt_ms = rtt;
            drop(peers);
            self.notify();
        }
    }

    fn lost(&self, id: EndpointId) {
        if let Some(live) = self.peers.lock().unwrap().get_mut(&id) {
            if live.client.take().is_none() && live.direct.is_none() {
                return;
            }
            live.direct = None;
            live.rtt_ms = None;
        }
        self.notify();
    }

    /// Фон: режим меняют и мимо агента (`ferry mode`, старый трей), сеть меняется сама.
    async fn background(self: Arc<Self>) {
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        let mut n = 0u64;
        loop {
            tick.tick().await;
            n += 1;
            let mode = mode::current_in(&self.dir);
            self.update_me(|s| s.mode = mode);
            if n.is_multiple_of(15)
                && let Some(lan) = &self.lan
            {
                lan.refresh_interfaces();
            }
        }
    }

    async fn handle(self: Arc<Self>, msg: DeviceMessage) {
        match msg {
            DeviceMessage::Watch(WithChannels { tx, .. }) => {
                let mut rx = self.me.subscribe();
                loop {
                    let status = rx.borrow_and_update().clone();
                    if tx.send(status).await.is_err() || rx.changed().await.is_err() {
                        break;
                    }
                }
            }
            DeviceMessage::SetRole(WithChannels {
                tx,
                inner: SetRole(role),
                ..
            }) => {
                tx.send(self.remote_change(|| self.set_role(role)))
                    .await
                    .ok();
            }
            DeviceMessage::Rename(WithChannels {
                tx,
                inner: Rename(name),
                ..
            }) => {
                let result = store::valid_name(&name)
                    .and_then(|name| self.remote_change(|| self.rename(name)));
                tx.send(result).await.ok();
            }
            other => self.node.clone().handle(other).await,
        }
    }
}

/// Протокол устройства — только для связанных устройств.
struct DeviceHandler(Arc<Inner>);

impl ProtocolHandler for DeviceHandler {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        if self
            .0
            .store
            .lock()
            .unwrap()
            .peer(conn.remote_id())
            .is_none()
        {
            conn.close(1u32.into(), b"not paired");
            return Ok(());
        }
        while let Some(msg) = irpc_iroh::read_request::<DeviceProtocol>(&conn).await? {
            tokio::spawn(self.0.clone().handle(msg));
        }
        Ok(())
    }
}

struct PairHandler(Arc<Inner>);

impl ProtocolHandler for PairHandler {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let inner = &self.0;
        let name = inner.store.lock().unwrap().name.clone();
        match pair::respond(&conn, inner.endpoint.id(), &name, &inner.pairing).await {
            Ok(peer_name) => inner.clone().add_peer(conn.remote_id(), peer_name),
            Err(e) => tracing::warn!("связывание с {}: {e:#}", conn.remote_id().fmt_short()),
        }
        // Окно могло закрыться: связали или кончились попытки.
        inner.announce();
        inner.notify();
        Ok(())
    }
}

impl fmt::Debug for DeviceHandler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DeviceHandler")
    }
}

impl fmt::Debug for PairHandler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PairHandler")
    }
}
