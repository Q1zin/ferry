//! Устройства ferry в локальной сети (mDNS). Рассылаем только на LAN-интерфейсах: у VPN свой
//! маршрут на multicast, и без явного интерфейса пакеты уходят в туннель (так ведёт себя
//! обёртка mDNS из iroh). Найденные адреса отдаём iroh — с ними он соединяется без интернета.

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use ferry_core::device::LanDevice;
use iroh::{EndpointAddr, EndpointId, TransportAddr, address_lookup::MemoryLookup};
use mdns_sd::{IfKind, ServiceDaemon, ServiceEvent, ServiceInfo};

const SERVICE: &str = "_ferry._udp.local.";

pub struct Lan {
    daemon: ServiceDaemon,
    me: EndpointId,
    port: u16,
    interfaces: Mutex<Vec<Ipv4Addr>>,
    seen: Arc<Mutex<BTreeMap<String, LanDevice>>>,
}

impl Lan {
    /// `port` — порт, на котором iroh принимает соединения; `on_change` — список изменился.
    pub fn start(
        me: EndpointId,
        port: u16,
        lookup: MemoryLookup,
        on_change: impl Fn() + Send + 'static,
    ) -> Result<Lan> {
        let lan = Lan {
            daemon: ServiceDaemon::new()?,
            me,
            port,
            interfaces: Mutex::default(),
            seen: Arc::default(),
        };
        lan.refresh_interfaces();
        let events = lan.daemon.browse(SERVICE)?;
        let seen = lan.seen.clone();
        std::thread::spawn(move || {
            while let Ok(event) = events.recv() {
                match event {
                    ServiceEvent::ServiceResolved(info) => {
                        let prop = |key| info.get_property_val_str(key).unwrap_or_default();
                        let Ok(id) = prop("id").parse::<EndpointId>() else {
                            continue;
                        };
                        if id == me {
                            continue;
                        }
                        let port = info.get_port();
                        let addrs = info
                            .get_addresses_v4()
                            .into_iter()
                            .map(|ip| TransportAddr::Ip(SocketAddr::new(ip.into(), port)));
                        lookup.add_endpoint_info(EndpointAddr::from_parts(id, addrs));
                        let device = LanDevice {
                            id: id.to_string(),
                            name: prop("name").into(),
                            pairing: prop("pair") == "1",
                        };
                        let old = seen
                            .lock()
                            .unwrap()
                            .insert(info.get_fullname().into(), device.clone());
                        if old != Some(device) {
                            on_change();
                        }
                    }
                    ServiceEvent::ServiceRemoved(_, fullname)
                        if seen.lock().unwrap().remove(&fullname).is_some() =>
                    {
                        on_change();
                    }
                    _ => {}
                }
            }
        });
        Ok(lan)
    }

    /// Объявить себя: имя и ждём ли связывания. Повторный вызов обновляет запись.
    pub fn announce(&self, name: &str, pairing: bool) -> Result<()> {
        let id = self.me.to_string();
        let instance = &id[..16];
        let info = ServiceInfo::new(
            SERVICE,
            instance,
            &format!("ferry-{instance}.local."),
            (),
            self.port,
            &[
                ("id", id.as_str()),
                ("name", name),
                ("pair", if pairing { "1" } else { "0" }),
            ][..],
        )?
        .enable_addr_auto();
        self.daemon.register(info)?;
        Ok(())
    }

    /// Перечитать LAN-интерфейсы: сменилась сеть, включили или выключили VPN.
    pub fn refresh_interfaces(&self) {
        let now = lan_ipv4();
        let mut current = self.interfaces.lock().unwrap();
        if *current == now {
            return;
        }
        let _ = self.daemon.disable_interface(IfKind::All);
        for ip in &now {
            let _ = self.daemon.enable_interface(IfKind::Addr((*ip).into()));
        }
        *current = now;
    }

    pub fn devices(&self) -> Vec<LanDevice> {
        let mut list: Vec<_> = self.seen.lock().unwrap().values().cloned().collect();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        list.dedup_by_key(|d| d.id.clone());
        list
    }
}

impl Drop for Lan {
    fn drop(&mut self) {
        let _ = self.daemon.shutdown();
    }
}

/// IPv4 локальных сетей — интерфейсы с широковещательным адресом. Туннели VPN — точка-точка,
/// широковещательного адреса у них нет.
pub fn lan_ipv4() -> Vec<Ipv4Addr> {
    let mut ips: Vec<Ipv4Addr> = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|iface| match iface.addr {
            if_addrs::IfAddr::V4(v4) if v4.broadcast.is_some() => Some(v4.ip),
            _ => None,
        })
        .filter(|ip| !ip.is_loopback() && !ip.is_link_local())
        .collect();
    ips.sort();
    ips.dedup();
    ips
}
