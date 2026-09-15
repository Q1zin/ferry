//! Протокол между связанными устройствами (irpc поверх iroh). Статус не опрашивают: `Watch`
//! присылает его сразу и потом каждое изменение. Остальное — команды устройству: оно выполняет
//! их у себя, в сессии пользователя, и отвечает разово или потоком.

use ferry_core::jobs::Job;
use iroh::endpoint::Connection;
use irpc::{
    Client,
    channel::{mpsc, oneshot},
    rpc_requests,
};
use irpc_iroh::IrohRemoteConnection;
use serde::{Deserialize, Serialize};

pub use ferry_core::device::{Dir, Fail, Role, Status};

pub const ALPN: &[u8] = b"ferry/device/1";

pub type Reply<T> = Result<T, Fail>;

/// Вывод команды или лога: куски stdout и stderr, в конце — код выхода или ошибка.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum Output {
    Out(Vec<u8>),
    Err(Vec<u8>),
    Exit(i32),
    Fail(Fail),
}

/// Кусок файла: снимок экрана, архив, git-бандл. `End` — файл целиком.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum Chunk {
    Data(Vec<u8>),
    End,
    Fail(Fail),
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Watch;

#[derive(Serialize, Deserialize, Debug)]
pub struct SetRole(pub Role);

#[derive(Serialize, Deserialize, Debug)]
pub struct Rename(pub String);

/// Команда шеллу устройства. `desk` — с окнами (GUI, тесты): в режиме «не трогать рабочий
/// стол» такие отклоняются.
#[derive(Serialize, Deserialize, Debug)]
pub struct Run {
    pub cmd: String,
    pub dir: Dir,
    pub desk: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Info;

#[derive(Serialize, Deserialize, Debug)]
pub struct Jobs;

/// Фоновая задача: живёт после вызова, пишет лог.
#[derive(Serialize, Deserialize, Debug)]
pub struct Start {
    pub name: String,
    pub cmd: String,
    pub dir: Dir,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Log {
    pub name: String,
    pub follow: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Kill {
    pub name: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Shot;

/// Окна и ввод: `windows`, `click x y [right|double]`, `type …`, `key …` — как `ferry input`.
#[derive(Serialize, Deserialize, Debug)]
pub struct Input {
    pub args: Vec<String>,
}

/// Запись экрана кадрами; ответ — архив кадров и листа-обзора.
#[derive(Serialize, Deserialize, Debug)]
pub struct Rec {
    pub seconds: String,
    pub fps: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Power {
    pub action: String,
}

/// Снять всё: команды и задачи.
#[derive(Serialize, Deserialize, Debug)]
pub struct Stop;

/// Подготовить проект к снимку: завести репозиторий, сказать, какой снимок уже есть, и отдать
/// правки поверх него (если они не совпадают с деревом `keep`).
#[derive(Serialize, Deserialize, Debug)]
pub struct SyncPrepare {
    pub project: String,
    pub keep: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Prepared {
    pub head: Option<String>,
    pub changes: Vec<u8>,
}

/// Новый снимок: git-бандл кусками, потом checkout.
#[derive(Serialize, Deserialize, Debug)]
pub struct SyncApply {
    pub project: String,
    pub commit: String,
}

/// Архив (tar) кусками — распаковать в `dir`.
#[derive(Serialize, Deserialize, Debug)]
pub struct Put {
    pub dir: Dir,
}

/// Файл или каталог `path` архивом: `~/…` — от дома, абсолютный, иначе — от `dir`.
#[derive(Serialize, Deserialize, Debug)]
pub struct Get {
    pub path: String,
    pub dir: Dir,
}

/// Одно TCP-соединение на `127.0.0.1:<port>` устройства: байты в обе стороны.
#[derive(Serialize, Deserialize, Debug)]
pub struct Tunnel {
    pub port: u16,
}

#[rpc_requests(message = DeviceMessage)]
#[derive(Serialize, Deserialize, Debug)]
pub enum DeviceProtocol {
    /// Статус сразу и затем каждое его изменение.
    #[rpc(tx = mpsc::Sender<Status>)]
    Watch(Watch),
    #[rpc(tx = oneshot::Sender<Result<(), String>>)]
    SetRole(SetRole),
    #[rpc(tx = oneshot::Sender<Result<(), String>>)]
    Rename(Rename),
    #[rpc(tx = mpsc::Sender<Output>)]
    Run(Run),
    #[rpc(tx = oneshot::Sender<Reply<Vec<(String, String)>>>)]
    Info(Info),
    #[rpc(tx = oneshot::Sender<Reply<Vec<Job>>>)]
    Jobs(Jobs),
    #[rpc(tx = oneshot::Sender<Reply<String>>)]
    Start(Start),
    #[rpc(tx = mpsc::Sender<Output>)]
    Log(Log),
    #[rpc(tx = oneshot::Sender<Reply<String>>)]
    Kill(Kill),
    #[rpc(tx = mpsc::Sender<Chunk>)]
    Shot(Shot),
    #[rpc(tx = oneshot::Sender<Reply<String>>)]
    Input(Input),
    #[rpc(tx = mpsc::Sender<Chunk>)]
    Rec(Rec),
    #[rpc(tx = oneshot::Sender<Reply<String>>)]
    Power(Power),
    #[rpc(tx = oneshot::Sender<Reply<String>>)]
    Stop(Stop),
    #[rpc(tx = oneshot::Sender<Reply<Prepared>>)]
    SyncPrepare(SyncPrepare),
    #[rpc(tx = oneshot::Sender<Reply<()>>, rx = mpsc::Receiver<Chunk>)]
    SyncApply(SyncApply),
    #[rpc(tx = oneshot::Sender<Reply<()>>, rx = mpsc::Receiver<Chunk>)]
    Put(Put),
    #[rpc(tx = mpsc::Sender<Chunk>)]
    Get(Get),
    #[rpc(tx = mpsc::Sender<Vec<u8>>, rx = mpsc::Receiver<Vec<u8>>)]
    Tunnel(Tunnel),
}

/// Клиент протокола поверх одного соединения с устройством.
#[derive(Clone, Debug)]
pub struct DeviceClient(pub(crate) Client<DeviceProtocol>);

/// Связь оборвалась посреди запроса.
pub(crate) fn lost(e: impl std::fmt::Display) -> Fail {
    Fail::new(format!(
        "связь с устройством оборвалась — повтори, когда оно снова на связи ({e})"
    ))
}

impl DeviceClient {
    pub fn new(conn: Connection) -> Self {
        Self(Client::boxed(IrohRemoteConnection::new(conn)))
    }

    pub async fn watch(&self) -> irpc::Result<mpsc::Receiver<Status>> {
        self.0.server_streaming(Watch, 4).await
    }

    pub async fn set_role(&self, role: Role) -> anyhow::Result<()> {
        self.0.rpc(SetRole(role)).await?.map_err(anyhow::Error::msg)
    }

    pub async fn rename(&self, name: String) -> anyhow::Result<()> {
        self.0.rpc(Rename(name)).await?.map_err(anyhow::Error::msg)
    }

    /// Разовый запрос: ответ устройства или ошибка связи.
    pub(crate) async fn ask<Req, T>(&self, req: Req) -> Reply<T>
    where
        DeviceProtocol: From<Req>,
        DeviceMessage: From<irpc::WithChannels<Req, DeviceProtocol>>,
        Req: irpc::Channels<
                DeviceProtocol,
                Tx = oneshot::Sender<Reply<T>>,
                Rx = irpc::channel::none::NoReceiver,
            >,
        T: irpc::RpcMessage,
    {
        self.0.rpc(req).await.map_err(lost)?
    }
}
