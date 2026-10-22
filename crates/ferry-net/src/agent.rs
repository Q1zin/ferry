//! Агент устройства: endpoint iroh, связь со всеми связанными устройствами и связывание
//! новых. С каждым связанным устройством держит соединение и подписку на его статус —
//! изменения приходят сами, обрыв виден сразу. Приложение и CLI работают через агента.

use std::collections::BTreeMap;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use ferry_core::device::{Activity, PairingInfo, PeerView, Snapshot};
use ferry_core::mode::{self, Mode};
use iroh::address_lookup::MemoryLookup;
use iroh::dns::{BoxIter, DnsError, DnsResolver, NameserverConfig, Resolver, TxtRecordData};
use iroh::endpoint::{Connection, QuicTransportConfig, presets};
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode};
use irpc::WithChannels;
use n0_future::boxed::BoxFuture;
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
    /// UDP-порт для соединений; `None` — любой свободный.
    pub port: Option<u16>,
    /// Обновить ferry на этом устройстве из релиза (умеет приложение): ответ — что будет; само
    /// обновление и перезапуск — после ответа.
    pub update: Option<UpdateHook>,
    pub update_file: Option<LocalUpdateHook>,
    pub update_status: ferry_core::update::Monitor,
}

pub type UpdateHook =
    Arc<dyn Fn() -> n0_future::boxed::BoxFuture<Result<String, String>> + Send + Sync>;

pub type LocalUpdateHook =
    Arc<dyn Fn(PathBuf, String) -> BoxFuture<Result<String, String>> + Send + Sync>;

/// Порт ferry по умолчанию.
pub const PORT: u16 = 47760;

impl Options {
    pub fn new(dir: PathBuf) -> Options {
        Options {
            dir,
            internet: true,
            lan: true,
            home: ferry_core::home().unwrap_or_default(),
            supervisor: std::env::current_exe().unwrap_or_default(),
            port: Some(PORT),
            update: None,
            update_file: None,
            update_status: Arc::new(Mutex::new(ferry_core::update::Status::default())),
        }
    }
}

#[derive(Clone)]
pub struct Agent {
    inner: Arc<Inner>,
    _router: Arc<Router>,
}

struct Tunnel {
    task: AbortHandle,
    owner: Option<String>,
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
    /// Журнал: что связанные устройства запрашивали на этом, последние — в конце.
    activity: Mutex<std::collections::VecDeque<Activity>>,
    /// Команды, которые это устройство выполняет для других.
    node: Arc<Node>,
    /// Открытые туннели: (устройство, порт).
    tunnels: Mutex<BTreeMap<(String, u16), Tunnel>>,
    /// Бронь рабочего стола этого устройства.
    desk: Mutex<crate::desk::Lease>,
    /// Кто сейчас действует на рабочем столе — для рамки управления.
    acting: Mutex<crate::desk::Acting>,
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
    /// Когда установлено соединение, через которое идут команды, и оно само.
    since: Option<std::time::Instant>,
    conn: Option<Connection>,
}

/// DNS: публичные серверы через HTTPS (Cloudflare, Google) и системный резолвер — наперегонки,
/// ответ — первый успешный. С VPN системный DNS отвечает ошибкой, которую резолвер считает
/// окончательной, — её не ждём, ждём HTTPS. В корпоративной сети наружу закрыт любой DNS, кроме
/// своего, — отвечает системный. Не ответил никто — ошибка публичных.
#[derive(Debug)]
struct Dns {
    public: DnsResolver,
    system: DnsResolver,
}

impl Dns {
    fn new() -> Dns {
        let https = ["1.1.1.1", "8.8.8.8"].map(|ip| NameserverConfig::https(ip.parse().unwrap()));
        Dns {
            // Закрыт HTTPS к ним — резолвер сам перейдёт к запасным публичным серверам.
            public: DnsResolver::builder().add_nameserver_configs(https).build(),
            system: DnsResolver::builder()
                .with_system_defaults()
                .disable_fallback()
                .build(),
        }
    }

    /// Один и тот же запрос к обоим резолверам.
    fn race<T: Send + 'static, F>(
        &self,
        lookup: impl Fn(DnsResolver) -> F,
    ) -> BoxFuture<Result<BoxIter<T>, DnsError>>
    where
        F: Future<Output = Result<Vec<T>, DnsError>> + Send + 'static,
    {
        let (public, system) = (lookup(self.public.clone()), lookup(self.system.clone()));
        Box::pin(async move {
            let found = first_ok(public, system).await?;
            Ok(Box::new(found.into_iter()) as BoxIter<T>)
        })
    }
}

/// Сколько ждать каждый резолвер; общий срок задаёт сам iroh.
const LOOKUP: Duration = Duration::from_secs(10);

impl Resolver for Dns {
    fn lookup_ipv4(&self, host: String) -> BoxFuture<Result<BoxIter<Ipv4Addr>, DnsError>> {
        self.race(move |r| {
            let host = host.clone();
            async move {
                let ips = r.lookup_ipv4(host, LOOKUP).await?;
                Ok(ips
                    .filter_map(|ip| match ip {
                        IpAddr::V4(ip) => Some(ip),
                        IpAddr::V6(_) => None,
                    })
                    .collect())
            }
        })
    }

    fn lookup_ipv6(&self, host: String) -> BoxFuture<Result<BoxIter<Ipv6Addr>, DnsError>> {
        self.race(move |r| {
            let host = host.clone();
            async move {
                let ips = r.lookup_ipv6(host, LOOKUP).await?;
                Ok(ips
                    .filter_map(|ip| match ip {
                        IpAddr::V6(ip) => Some(ip),
                        IpAddr::V4(_) => None,
                    })
                    .collect())
            }
        })
    }

    fn lookup_txt(&self, host: String) -> BoxFuture<Result<BoxIter<TxtRecordData>, DnsError>> {
        self.race(move |r| {
            let host = host.clone();
            async move { Ok(r.lookup_txt(host, LOOKUP).await?.collect()) }
        })
    }

    fn clear_cache(&self) {
        self.public.clear_cache();
        self.system.clear_cache();
    }

    /// Сеть сменилась (уехал из офиса, включил VPN): системные серверы — заново.
    fn reset(&self) -> Box<dyn Resolver> {
        Box::new(Dns::new())
    }
}

/// Первый успешный из двух ответов; оба с ошибкой — ошибка первого.
async fn first_ok<T, E>(
    public: impl Future<Output = Result<T, E>>,
    system: impl Future<Output = Result<T, E>>,
) -> Result<T, E> {
    tokio::pin!(public, system);
    tokio::select! {
        found = &mut public => match found {
            Ok(found) => Ok(found),
            Err(e) => system.await.map_err(|_| e),
        },
        found = &mut system => match found {
            Ok(found) => Ok(found),
            Err(_) => public.await,
        },
    }
}

impl Agent {
    pub async fn start(opts: Options) -> Result<Agent> {
        let key = store::key(&opts.dir)?;
        let store = Store::load(&opts.dir)?;
        let lookup = MemoryLookup::new();
        let builder = |port: Option<u16>| -> Result<iroh::endpoint::Builder> {
            let mut builder = if opts.internet {
                Endpoint::builder(presets::N0).dns_resolver(DnsResolver::custom(Dns::new()))
            } else {
                Endpoint::builder(presets::Minimal).relay_mode(RelayMode::Disabled)
            };
            if let Some(port) = port {
                builder = builder.bind_addr(("0.0.0.0", port))?;
            }
            // Пропавшее устройство (выключили, ушло из сети) видно через 12 с, а не через 30 по
            // умолчанию: iroh шлёт heartbeat раз в 5 с, один пропуск переживём.
            let transport = QuicTransportConfig::builder()
                .max_idle_timeout(Some(Duration::from_secs(12).try_into()?))
                .build();
            Ok(builder
                .transport_config(transport)
                .secret_key(key.clone())
                .alpns(vec![proto::ALPN.to_vec(), pair::ALPN.to_vec()])
                .address_lookup(lookup.clone()))
        };
        // Постоянный порт: перезапущенное устройство оказывается на том же адресе, и другие
        // сразу снова до него достают — iroh шлёт на путь, который уже знает. Порт занят (второй
        // экземпляр) — любой свободный.
        let endpoint = match builder(opts.port)?.bind().await {
            Ok(endpoint) => endpoint,
            Err(e) if opts.port.is_some() => {
                tracing::warn!("порт {:?} занят ({e}) — беру любой", opts.port);
                builder(None)?.bind().await?
            }
            Err(e) => return Err(e.into()),
        };

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
        let (relay, relay_listener) = crate::relay::Local::bind(&opts.dir, &peers).await?;
        let relay = Arc::new(relay);
        let node = Arc::new(Node {
            relay: relay.clone(),
            update: opts.update.clone(),
            update_file: opts.update_file.clone(),
            update_status: opts.update_status.clone(),
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
            desk: Mutex::default(),
            acting: Mutex::default(),
            peers: Mutex::default(),
            lan,
            pairing: Mutex::default(),
            changed,
            activity: Mutex::default(),
        });
        let router = Router::builder(endpoint)
            .accept(proto::ALPN, DeviceHandler(inner.clone()))
            .accept(pair::ALPN, PairHandler(inner.clone()))
            .spawn();
        // Вход релея git: сессия `ferry ssh` уходит к устройству, запустившему команду.
        let clients = inner.clone();
        tokio::spawn(relay.serve(relay_listener, move |id| clients.client(id).ok()));
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
            activity: inner.activity.lock().unwrap().iter().cloned().collect(),
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
            version: peer.status.as_ref().map(|s| s.version.clone()),
            os: peer.status.as_ref().map(|s| s.os.clone()),
        })
    }

    /// `127.0.0.1:<port>` здесь → тот же порт устройства (CDP, dev-сервер). Порт тот же с обеих
    /// сторон: CDP отдаёт в `/json/version` ws-адрес со своим портом.
    pub async fn open_tunnel(&self, who: &str, port: u16) -> Result<String, Fail> {
        self.open_tunnel_owned(who, port, None).await
    }
    pub async fn open_tunnel_owned(
        &self,
        who: &str,
        port: u16,
        owner: Option<String>,
    ) -> Result<String, Fail> {
        let device = self.device(who)?;
        let key = (device.name.clone(), port);
        if let Some(existing) = self.inner.tunnels.lock().unwrap().get(&key) {
            if owner != existing.owner {
                return Err(Fail::refused("туннель уже принадлежит другой сессии"));
            }
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
            let mut streams = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted=listener.accept()=>{
                        let Ok((stream,_))=accepted else{break};
                        let Ok(device)=agent.device(&name)else{continue;};
                        streams.spawn(async move {if let Ok((tx,rx))=device.tunnel(port).await{crate::node::pipe(stream,tx,rx).await;}});
                    },
                    _=streams.join_next(),if !streams.is_empty()=>{}
                }
            }
        });
        self.inner.tunnels.lock().unwrap().insert(
            key,
            Tunnel {
                task: task.abort_handle(),
                owner,
            },
        );
        Ok(format!("ferry: 127.0.0.1:{port} → {}:{port}", device.name))
    }

    pub fn close_tunnel(&self, who: &str, port: u16) -> Result<String, Fail> {
        let mut tunnels = self.inner.tunnels.lock().unwrap();
        let key = tunnels
            .keys()
            .find(|(name, p)| *p == port && (name == who || who.is_empty()))
            .cloned();
        match key.and_then(|key| tunnels.remove(&key).map(|task| (key, task))) {
            Some(((name, port), tunnel)) => {
                tunnel.task.abort();
                Ok(format!("ferry: туннель {name}:{port} закрыт"))
            }
            None => Err(Fail::new(format!("туннеля {who}:{port} нет"))),
        }
    }

    /// Открытые туннели: устройство и порт.
    pub fn close_tunnel_owned(&self, who: &str, port: u16, owner: &str) -> Result<bool, Fail> {
        let mut tunnels = self.inner.tunnels.lock().unwrap();
        let key = tunnels
            .keys()
            .find(|(name, p)| *p == port && name == who)
            .cloned();
        if let Some(key) = key
            && tunnels
                .get(&key)
                .is_some_and(|tunnel| tunnel.owner.as_deref() == Some(owner))
            && let Some(tunnel) = tunnels.remove(&key)
        {
            tunnel.task.abort();
            return Ok(true);
        }

        Ok(false)
    }

    pub fn tunnels(&self) -> Vec<(String, u16)> {
        self.inner.tunnels.lock().unwrap().keys().cloned().collect()
    }

    /// Кто сейчас действует на рабочем столе этого устройства (и только что закончил).
    pub fn acting(&self) -> Option<String> {
        let acting = self.inner.acting.lock().unwrap();
        acting
            .who(std::time::Instant::now())
            .map(|who| match self.desk_note() {
                Some(note) => format!("{who} · {note}"),
                None => who,
            })
    }

    /// Обновить ferry на этом устройстве (`ferry update` без целей).
    pub fn logs_self(&self, lines: usize) -> Result<String, Fail> {
        self.inner.node.logs(lines)
    }

    pub fn desk_self(
        &self,
        session: &str,
        action: crate::proto::DeskAction,
    ) -> Result<String, Fail> {
        let result = self
            .inner
            .desk
            .lock()
            .unwrap()
            .apply(
                self.id(),
                session,
                action,
                std::time::Instant::now(),
                |id| {
                    if id == self.id() {
                        self.snapshot().me.name
                    } else {
                        self.inner
                            .store
                            .lock()
                            .unwrap()
                            .peer(id)
                            .map(|p| p.name.clone())
                            .unwrap_or_else(|| id.fmt_short().to_string())
                    }
                },
            )
            .map_err(Fail::refused);
        self.inner.notify();
        result
    }

    pub fn desk_note(&self) -> Option<String> {
        self.inner
            .desk
            .lock()
            .unwrap()
            .note(std::time::Instant::now())
    }
    pub async fn screen_stream_self(
        &self,
        request: ferry_core::screen::live::Request,
    ) -> irpc::channel::mpsc::Receiver<crate::proto::Reply<ferry_core::screen::live::Frame>> {
        let (tx, rx) = irpc::channel::mpsc::channel(1);
        let node = self.inner.node.clone();
        tokio::spawn(async move {
            node.screen_stream(request, tx).await;
        });
        rx
    }

    pub async fn storage_self(
        &self,
        request: ferry_core::storage::Request,
    ) -> Result<String, Fail> {
        self.inner.node.storage(request, false).await
    }
    pub async fn human_self(
        &self,
        request: ferry_core::human::Request,
        session: &str,
    ) -> Result<String, Fail> {
        self.inner
            .node
            .human(
                request,
                format!("{}:{session}", self.id()),
                self.snapshot().me.name,
                false,
            )
            .await
    }

    pub async fn atomic_start_self(
        &self,
        request: ferry_core::jobs::execution::StartRequest,
        cwd: Option<PathBuf>,
    ) -> Result<ferry_core::jobs::execution::Started, Fail> {
        self.inner.node.atomic_start(request, cwd, None).await
    }
    pub async fn execution_cancel_self(&self, id: String, session: String) -> Result<String, Fail> {
        self.inner.node.execution_cancel(id, session, false).await
    }
    pub async fn execution_cancel_owned_self(
        &self,
        id: String,
        session: String,
    ) -> Result<String, Fail> {
        self.inner
            .node
            .execution_cancel_owned(id, session, false)
            .await
    }
    pub async fn execution_lookup_self(
        &self,
        id: String,
    ) -> Result<Option<ferry_core::jobs::execution::Started>, Fail> {
        self.inner.node.execution_lookup(id).await
    }
    pub async fn act_v2_self(
        &self,
        request: ferry_core::screen::act::RequestV2,
    ) -> Result<ferry_core::screen::act::ReportV2, Fail> {
        self.inner.node.act_v2(request, false).await
    }
    pub async fn observe_v2_self(
        &self,
        request: ferry_core::screen::observe::RequestV2,
    ) -> Result<ferry_core::screen::observe::ObservationV2, Fail> {
        self.inner.node.observe_v2(request, false).await
    }

    pub async fn act_self(
        &self,
        request: ferry_core::screen::act::Request,
    ) -> Result<ferry_core::screen::act::Report, Fail> {
        self.inner.node.act(request, false).await
    }
    pub async fn observe_self(
        &self,
        request: ferry_core::screen::observe::Request,
    ) -> Result<ferry_core::screen::observe::Observation, Fail> {
        self.inner.node.observe(request, false).await
    }
    pub async fn evidence_self(
        &self,
        query: ferry_core::jobs::evidence::Query,
    ) -> Result<String, Fail> {
        self.inner.node.evidence(query, false).await
    }

    pub async fn job_result_self(
        &self,
        request: crate::proto::JobResult,
    ) -> Result<ferry_core::jobs::JobResult, Fail> {
        self.inner.node.job_result(request, false).await
    }

    pub async fn update_self(&self) -> Result<String, Fail> {
        self.inner.node.update().await
    }

    pub async fn update_file_self(&self, file: PathBuf, signature: String) -> Result<String, Fail> {
        self.inner.node.install_file(file, signature).await
    }
    pub fn update_status(&self) -> ferry_core::update::Status {
        self.inner.node.update_status.lock().unwrap().clone()
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

    /// Устройство заново подключилось к нам — значит, оно перезапускалось или меняло сеть, и
    /// наше соединение к нему, скорее всего, мёртвое: без этого оно числилось бы живым до
    /// таймаута (12 с), и команды в это время падали бы. Свежее (моложе 5 с) не трогаем — так
    /// встречные переподключения не ходят по кругу.
    fn refresh(self: Arc<Self>, id: EndpointId) {
        {
            let mut peers = self.peers.lock().unwrap();
            let Some(live) = peers.get_mut(&id) else {
                return;
            };
            if live
                .since
                .is_none_or(|since| since.elapsed() < Duration::from_secs(5))
            {
                return;
            }
            if let Some(task) = live.task.take() {
                task.abort();
            }
            // Запросы, ушедшие в старое соединение, падают сразу, а не ждут таймаута.
            if let Some(conn) = live.conn.take() {
                conn.close(0u32.into(), b"replaced");
            }
            live.client = None;
            live.since = None;
        }
        self.notify();
        self.keep(id);
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
            // irpc читает кадр частями: таймер не должен бросать незавершённое чтение.
            let next = statuses.recv();
            tokio::pin!(next);
            let status = loop {
                tokio::select! {
                    next = &mut next => break next?,
                    _ = tick.tick() => self.on_path(id, &conn),
                }
            };
            match status {
                Some(status) => self.on_status(id, status, client.take(), &conn),
                None => return Ok(()),
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
                live.since = Some(std::time::Instant::now());
                live.conn = Some(conn.clone());
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
            live.since = None;
            live.conn = None;
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

    async fn handle(self: Arc<Self>, msg: DeviceMessage, from: EndpointId) {
        // Действие на рабочем столе идёт, пока его обрабатываем: `run --desk` — до конца команды.
        let _acting = describe(&msg).and_then(|(what, desk)| {
            let who = self.note(from, what, desk);
            desk.then(|| Acting::new(&self, who))
        });
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
            DeviceMessage::Human(WithChannels { inner, tx, .. }) => {
                let label = self
                    .store
                    .lock()
                    .unwrap()
                    .peer(from)
                    .map(|p| p.name.clone())
                    .unwrap_or_else(|| from.fmt_short().to_string());
                let label = if inner.session.is_empty() {
                    label
                } else {
                    format!("{label} ({})", inner.session)
                };
                let result = self
                    .node
                    .human(
                        inner.request,
                        format!("{from}:{}", inner.session),
                        label,
                        true,
                    )
                    .await;
                self.notify();
                tx.send(result).await.ok();
            }
            DeviceMessage::Desk(WithChannels { tx, inner, .. }) => {
                let name = |id: EndpointId| {
                    let store = self.store.lock().unwrap();
                    store
                        .peer(id)
                        .map_or_else(|| id.fmt_short().to_string(), |p| p.name.clone())
                };
                let result = self
                    .desk
                    .lock()
                    .unwrap()
                    .apply(
                        from,
                        &inner.session,
                        inner.action,
                        std::time::Instant::now(),
                        name,
                    )
                    .map_err(|why| {
                        let me = self.me.borrow().name.clone();
                        Fail::refused(
                            format!("рабочий стол {me} {why}").replace("<устройство>", &me),
                        )
                    });
                self.notify();
                tx.send(result).await.ok();
            }
            other => self.node.clone().handle(other, from).await,
        }
    }
}

/// Сколько записей журнала держать.
const JOURNAL: usize = 100;

impl Inner {
    /// Записать запрос в журнал (и в лог) и показать его: окно, трей.
    fn note(&self, from: EndpointId, what: String, desk: bool) -> String {
        let who = self
            .store
            .lock()
            .unwrap()
            .peer(from)
            .map_or_else(|| from.fmt_short().to_string(), |p| p.name.clone());
        tracing::info!("{who}: {what}");
        let mut journal = self.activity.lock().unwrap();
        if journal.len() == JOURNAL {
            journal.pop_front();
        }
        journal.push_back(Activity {
            at: ferry_core::now(),
            who: who.clone(),
            what,
            desk,
        });
        drop(journal);
        self.notify();
        who
    }
}

/// Пока жив — `who` действует на рабочем столе: рамка управления горит.
struct Acting<'a>(&'a Inner, String);

impl<'a> Acting<'a> {
    fn new(inner: &'a Inner, who: String) -> Self {
        inner.acting.lock().unwrap().start(who.clone());
        inner.notify();
        Acting(inner, who)
    }
}

impl Drop for Acting<'_> {
    fn drop(&mut self) {
        let mut acting = self.0.acting.lock().unwrap();
        acting.finish(&self.1, std::time::Instant::now());
        drop(acting);
        self.0.notify();
    }
}

/// Запрос — строкой для журнала, и трогает ли он рабочий стол; `None` — не пишем (статус,
/// чтение, туннели — их по соединению на каждый запрос CDP).
fn describe(msg: &DeviceMessage) -> Option<(String, bool)> {
    let line = |cmd: &str| {
        cmd.lines()
            .next()
            .unwrap_or_default()
            .chars()
            .take(120)
            .collect::<String>()
    };
    Some(match msg {
        DeviceMessage::RunShell(m) => (
            format!("команда: {}", line(&m.inner.run.cmd)),
            m.inner.run.desk,
        ),
        DeviceMessage::StartShell(m) => (
            format!(
                "задача {}: {}",
                m.inner.start.name,
                line(&m.inner.start.cmd)
            ),
            true,
        ),
        DeviceMessage::Run(m) => (format!("команда: {}", line(&m.inner.cmd)), m.inner.desk),
        DeviceMessage::Start(m) => (
            format!("задача {}: {}", m.inner.name, line(&m.inner.cmd)),
            true,
        ),
        DeviceMessage::Kill(m) => (format!("остановить задачу {}", m.inner.name), false),
        DeviceMessage::Stop(_) => ("снять все команды и задачи".into(), false),
        // Набранный текст в журнал и лог не пишем: там бывают пароли тестовых входов.
        DeviceMessage::Input(m) => match m.inner.args.first().map(String::as_str) {
            Some("type") => ("ввод текста".into(), true),
            Some("clip") if m.inner.args.len() > 1 => ("буфер обмена: записать".into(), true),
            Some("clip") => ("буфер обмена: прочитать".into(), true),
            Some("windows" | "window" | "ui" | "probe" | "probe-window" | "probe-text") => {
                return None;
            }
            Some("text") => ("текст с экрана".into(), true),
            _ => (format!("ввод: {}", m.inner.args.join(" ")), true),
        },
        DeviceMessage::AtomicStart(m) => (
            format!(
                "задача {}: {}",
                m.inner.request.name,
                line(&m.inner.request.command)
            ),
            m.inner.request.resource == ferry_core::jobs::execution::Resource::Desktop,
        ),
        DeviceMessage::Human(m) => match &m.inner.request {
            ferry_core::human::Request::Ask { .. } => ("вопрос человеку".into(), false),
            _ => return None,
        },
        DeviceMessage::ActV2(_) => ("проверяемое действие".into(), true),
        DeviceMessage::ObserveV2(_) => ("наблюдение окна".into(), true),
        DeviceMessage::Act(_) => ("проверяемое действие".into(), true),
        DeviceMessage::Observe(_) => ("наблюдение окна".into(), true),
        DeviceMessage::ScreenStream(_) => ("трансляция экрана".into(), true),
        DeviceMessage::Shot(_) => ("снимок экрана".into(), true),
        DeviceMessage::ShotArea(_) => ("снимок части экрана".into(), true),
        DeviceMessage::ShotWindow(m) => (format!("снимок окна {}", m.inner.name), true),
        DeviceMessage::Rec(m) => (format!("запись экрана, {} с", m.inner.seconds), true),
        DeviceMessage::Power(m) => (format!("питание: {}", m.inner.action), false),
        DeviceMessage::SyncApply(m) => (format!("снимок проекта {}", m.inner.project), false),
        DeviceMessage::Put(_) => ("файлы в проект".into(), false),
        DeviceMessage::Get(m) => (format!("забрать {}", m.inner.path), false),
        DeviceMessage::Update(_) | DeviceMessage::UpdateFile(_) => ("обновить ferry".into(), false),
        DeviceMessage::SetRole(m) => (format!("роль: {}", m.inner.0.name()), false),
        DeviceMessage::Rename(m) => (format!("имя: {}", m.inner.0), false),
        _ => return None,
    })
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
        let from = conn.remote_id();
        self.0.clone().refresh(from);
        // Забыли устройство — рвём и его соединение сюда, вместе с подпиской на статус.
        let inner = self.0.clone();
        let watched = conn.clone();
        let forgotten = tokio::spawn(async move {
            let mut changed = inner.changed.subscribe();
            while changed.changed().await.is_ok() {
                if inner.store.lock().unwrap().peer(from).is_none() {
                    watched.close(1u32.into(), b"not paired");
                    return;
                }
            }
        });
        let result = async {
            loop {
                match irpc_iroh::read_request::<DeviceProtocol>(&conn).await {
                    Ok(Some(msg)) => {
                        tokio::spawn(self.0.clone().handle(msg, from));
                    }
                    Ok(None) => return Ok(()),
                    // Запрос из версии новее этой: отказать только ему, связь и статус не рвать.
                    Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                        tracing::warn!(
                            "{}: незнакомый запрос (ferry там новее?): {e}",
                            from.fmt_short()
                        );
                    }
                    Err(e) => return Err(e.into()),
                }
            }
        }
        .await;
        forgotten.abort();
        result
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

#[cfg(test)]
mod tests {
    use super::*;

    async fn agent(name: &str) -> Agent {
        let dir = std::env::temp_dir().join(format!("ferry-agent-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("home")).unwrap();
        let options = Options {
            internet: false,
            lan: false,
            home: dir.join("home"),
            port: None,
            ..Options::new(dir)
        };
        Agent::start(options).await.unwrap()
    }

    #[tokio::test]
    async fn first_successful_dns_answer_wins() {
        let late = |found: Result<u8, &'static str>| async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            found
        };
        // VPN: системный быстро отвечает ошибкой — ждём HTTPS.
        assert_eq!(first_ok(late(Ok(1)), async { Err("vpn") }).await, Ok(1));
        // Офис: HTTPS наружу закрыт — отвечает системный.
        assert_eq!(first_ok(late(Err("timeout")), async { Ok(2) }).await, Ok(2));
        assert_eq!(
            first_ok(late(Err("public")), async { Err("system") }).await,
            Err("public")
        );
        // Ответ есть — второго не ждём.
        assert_eq!(
            first_ok(async { Ok(3) }, std::future::pending::<Result<u8, &str>>()).await,
            Ok(3)
        );
    }

    #[tokio::test]
    async fn path_tick_does_not_drop_a_partial_status() {
        let a = agent("partial-status").await;
        let server = Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Disabled)
            .alpns(vec![proto::ALPN.to_vec()])
            .bind()
            .await
            .unwrap();
        let id = server.id();
        a.learn(server.addr());
        a.inner.peers.lock().unwrap().insert(id, Live::default());
        let writer = tokio::spawn(async move {
            let conn = server.accept().await.unwrap().await.unwrap();
            let (mut tx, mut rx) = conn.accept_bi().await.unwrap();
            assert_eq!(rx.read_to_end(64).await.unwrap(), [1, 0]); // Watch
            // Postcard Status: node, Both, Active, macos, 0.1.18. Разрыв посреди тела кадра.
            let status = b"\x04node\x00\x00\x05macos\x060.1.18";
            tx.write_all(&[status.len() as u8, status[0]])
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(5500)).await;
            tx.write_all(&status[1..]).await.unwrap();
            tx.finish().unwrap();
            let _ = tokio::time::timeout(Duration::from_secs(5), conn.closed()).await;
            server.close().await;
        });
        let result = tokio::time::timeout(Duration::from_secs(10), a.inner.watch_peer(id)).await;
        assert!(
            matches!(result, Ok(Ok(()))),
            "status was interrupted: {result:?}"
        );
        assert_eq!(
            a.inner.peers.lock().unwrap()[&id]
                .status
                .as_ref()
                .unwrap()
                .name,
            "node"
        );
        a.shutdown().await;
        writer.await.unwrap();
    }

    // Устройство новее прислало запрос, которого эта версия не знает: отказать надо только ему,
    // а связь и подписку на статус не рвать.
    #[tokio::test]
    async fn unknown_request_keeps_the_link() {
        let (a, b) = (agent("new").await, agent("old").await);
        a.learn(b.addr());
        b.learn(a.addr());
        a.pair(&b.open_pairing().ticket).await.unwrap();
        let id = b.addr().id;
        let conn = loop {
            let live = a
                .inner
                .peers
                .lock()
                .unwrap()
                .get(&id)
                .map(|l| (l.client.is_some(), l.conn.clone()));
            if let Some((true, Some(conn))) = live {
                break conn;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };

        // Кадр irpc: длина, затем postcard; вариант 127 — такого запроса нет.
        let (mut send, mut recv) = conn.open_bi().await.unwrap();
        send.write_all(&[1, 0x7f]).await.unwrap();
        send.finish().unwrap();
        let _ = recv.read_to_end(64).await;

        let device = a.device(&id.to_string()).unwrap();
        let info = tokio::time::timeout(Duration::from_secs(5), device.info()).await;
        assert!(
            matches!(info, Ok(Ok(_))),
            "после незнакомого запроса связь жива: {info:?}"
        );
        assert!(conn.close_reason().is_none(), "соединение то же");
    }
}
