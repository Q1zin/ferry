//! Агенты в одном процессе: связывание, события, роли, отказы, переподключение.

mod common;

use std::time::Duration;

use common::*;
use ferry_net::proto::{self, DeviceClient};
use ferry_net::{Agent, Mode, Role, Snapshot};

#[tokio::test]
async fn paired_devices_push_status_and_obey_each_other() {
    let (a, b) = paired("mac", "win").await;
    let peer = |s: &Snapshot| s.peers[0].status.clone().unwrap();

    // Изменения приходят сами: без опроса, за доли секунды.
    b.set_role(None, Role::Node).await.unwrap();
    let took = until(&a, "роль b — узел", |s| {
        peer(s).role == Role::Node
    })
    .await;
    assert!(took < Duration::from_secs(1), "событие шло {took:?}");

    // Роль и имя меняются с другого устройства.
    a.set_role(Some(b.id()), Role::Console).await.unwrap();
    assert_eq!(b.snapshot().me.role, Role::Console);
    a.rename(Some(b.id()), "win-11").await.unwrap();
    until(&a, "b переименован", |s| {
        s.peers[0].name == "win-11"
    })
    .await;
    assert!(a.rename(Some(b.id()), "  ").await.is_err());

    // Режим выбирает человек за устройством: «отключён» запрещает менять его издалека.
    b.set_mode(Mode::Paused).unwrap();
    until(&a, "b отключён", |s| peer(s).mode == Mode::Paused).await;
    let err = a
        .set_role(Some(b.id()), Role::Both)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("отключён"), "{err}");
    assert_eq!(b.snapshot().me.role, Role::Console);

    // Связь обеих сторон после перезапуска — из сохранённого списка.
    b.shutdown().await;
    until(&a, "b ушёл", |s| !s.peers[0].online).await;
    let b2 = Agent::start(options("win")).await.unwrap();
    assert_eq!(b2.id(), b.id(), "ключ тот же");
    a.learn(b2.addr());
    b2.learn(a.addr());
    until(&a, "b вернулся", |s| s.peers[0].online).await;
    until(&b2, "b2 видит a", |s| {
        s.peers.first().is_some_and(|p| p.online)
    })
    .await;

    a.forget(b2.id()).unwrap();
    assert!(a.snapshot().peers.is_empty());
}

#[tokio::test]
async fn wrong_codes_burn_the_attempts() {
    let (a, b) = (agent("a1").await, agent("b1").await);
    a.learn(b.addr());
    let right = b.open_pairing();
    let wrong = right.ticket.replace(
        &right.code,
        if right.code == "0000-0000" {
            "1111-1111"
        } else {
            "0000-0000"
        },
    );
    for _ in 0..5 {
        let err = a.pair(&wrong).await.unwrap_err().to_string();
        assert!(err.contains("не подошёл"), "{err}");
    }
    let err = a.pair(&right.ticket).await.unwrap_err().to_string();
    assert!(err.contains("не ждёт связывания"), "{err}");
    assert!(b.snapshot().pairing.is_none());
    assert!(a.snapshot().peers.is_empty() && b.snapshot().peers.is_empty());
}

#[tokio::test]
async fn strangers_get_nothing() {
    let (_a, b) = paired("a2", "b2").await;
    let stranger = agent("stranger").await;
    let conn = iroh::Endpoint::builder(iroh::endpoint::presets::Minimal)
        .relay_mode(iroh::RelayMode::Disabled)
        .bind()
        .await
        .unwrap()
        .connect(b.addr(), proto::ALPN)
        .await
        .unwrap();
    let got = match DeviceClient::new(conn).watch().await {
        Ok(mut rx) => rx.recv().await.ok().flatten(),
        Err(_) => None,
    };
    assert!(
        got.is_none(),
        "несвязанному устройству отдали статус: {got:?}"
    );
    assert!(stranger.set_role(Some(b.id()), Role::Node).await.is_err());
}

#[tokio::test]
async fn pairing_once_introduces_everyone() {
    // Две пары: a–b и c–d. Связали b и c — и знакомы все четверо.
    let (a, b) = paired("mesh-a", "mesh-b").await;
    let (c, d) = paired("mesh-c", "mesh-d").await;
    let all = [&a, &b, &c, &d];
    // Адреса в тестах дают руками: поиска по ключу без интернета нет.
    for x in all {
        for y in all {
            x.learn(y.addr());
        }
    }
    pair(&b, &c).await;
    for x in all {
        until(
            x,
            "знает всех троих, все на связи",
            |s| s.peers.len() == 3 && s.peers.iter().all(|p| p.online),
        )
        .await;
    }

    // Забыли d на a — забыли все, и списки друг друга его не вернут.
    a.forget(d.id()).unwrap();
    let knows_d = |s: &Snapshot| s.peers.iter().any(|p| p.id == d.id().to_string());
    for x in [&a, &b, &c] {
        until(x, "d забыт", |s| !knows_d(s) && s.peers.len() == 2).await;
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(!knows_d(&a.snapshot()), "d вернулся из чужого списка");
    until(&d, "d отключён от всех", |s| {
        s.peers.iter().all(|p| !p.online)
    })
    .await;

    // Связали снова — снова у всех.
    pair(&a, &d).await;
    for x in [&a, &b, &c] {
        until(x, "d снова связан", |s| {
            s.peers
                .iter()
                .any(|p| p.id == d.id().to_string() && p.online)
        })
        .await;
    }
}
