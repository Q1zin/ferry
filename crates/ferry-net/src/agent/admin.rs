use std::sync::{Arc, Mutex, TryLockError};
use std::time::{Duration, Instant};

use ferry_core::admin_desktop::Broker;
use ferry_core::screen::{Window, admin};
use serde_json::{Value, json};

use super::{Agent, EndpointId, Fail, Inner, mode};
use crate::desk::Owner;

const MAX_GRANT: Duration = Duration::from_secs(15 * 60);

#[derive(Default)]
pub(super) struct State {
    selection: Option<Selection>,
    pending: Option<String>,
    grant: Option<Grant>,
}

struct Selection {
    owner: Owner,
    windows: Vec<Window>,
    until: Instant,
}

struct Grant {
    owner: Owner,
    window: Option<Window>,
    until: Option<Instant>,
    broker: Arc<Mutex<Broker>>,
}

impl Grant {
    fn all_windows(&self) -> bool {
        self.window.is_none()
    }

    fn matches_owner(&self, owner: &Owner) -> bool {
        if self.all_windows() {
            self.owner.from == owner.from
        } else {
            &self.owner == owner
        }
    }
}

impl Inner {
    fn admin_owner(&self) -> Option<(Owner, Instant)> {
        if !self.me.borrow().role.accepts_commands()
            || !mode::current_in(&self.dir).allows_desktop()
        {
            return None;
        }
        let owner = self.desk.lock().unwrap().owner(Instant::now())?;
        // Локальный пользователь подтверждает доступ именно для удалённой брони.
        (owner.0.from != self.endpoint.id()).then_some(owner)
    }

    fn owner_name(&self, owner: &Owner) -> String {
        self.store
            .lock()
            .unwrap()
            .peer(owner.from)
            .map_or_else(|| owner.from.fmt_short().to_string(), |p| p.name.clone())
    }

    pub(super) fn expire_admin(&self) {
        let has_grant = self.admin.lock().unwrap().grant.is_some();
        if !has_grant {
            return;
        }
        let owner = self.admin_owner();
        let desktop_available = admin::desktop_available();
        let mut state = self.admin.lock().unwrap();
        if state.grant.as_ref().is_some_and(|grant| {
            grant.until.is_some_and(|until| {
                !desktop_available
                    || until <= Instant::now()
                    || owner
                        .as_ref()
                        .is_none_or(|(owner, _)| !grant.matches_owner(owner))
            }) || match grant.broker.try_lock() {
                Ok(broker) => !broker.is_active(),
                Err(TryLockError::Poisoned(_)) => true,
                Err(TryLockError::WouldBlock) => false,
            }
        }) {
            let expired = state.grant.take();
            drop(state);
            drop(expired);
            self.notify();
        }
    }

    fn admin_report(&self, windows: Option<&[Window]>) -> Value {
        self.expire_admin();
        let owner = self.admin_owner().map(
            |(owner, _)| json!({ "device": self.owner_name(&owner), "session": owner.session }),
        );
        let state = self.admin.lock().unwrap();
        let approved_controller = state
            .grant
            .as_ref()
            .filter(|grant| grant.all_windows())
            .map(|grant| grant.owner.clone());
        let window = state.grant.as_ref().and_then(|grant| grant.window.clone());
        let pending = state.pending.is_some();
        drop(state);
        json!({
            "owner": owner,
            "window": window,
            "all_windows": approved_controller.is_some(),
            "approved_controller": approved_controller.as_ref().map(|owner| self.owner_name(owner)),
            "pending": pending,
            "candidates": windows,
        })
    }

    fn broker(&self, from: EndpointId, session: &str) -> Result<(Arc<Mutex<Broker>>, bool), Fail> {
        if !admin::desktop_available() {
            return Err(Fail::refused(
                "рабочий стол Windows заблокирован или защищён",
            ));
        }
        let (owner, _) = self
            .admin_owner()
            .ok_or_else(|| Fail::refused("сначала займи рабочий стол через ferry desk take"))?;
        let state = self.admin.lock().unwrap();
        state
            .grant
            .as_ref()
            .filter(|grant| {
                grant.until.is_none_or(|until| until > Instant::now())
                    && grant.matches_owner(&owner)
                    && owner.from == from
                    && owner.session == session
            })
            .map(|grant| (grant.broker.clone(), grant.all_windows()))
            .ok_or_else(|| {
                Fail::refused("владелец Windows должен разрешить управление окном администратора")
            })
    }

    pub(super) async fn execute_admin(
        self: Arc<Self>,
        from: EndpointId,
        session: String,
        window: Option<i64>,
        action: admin::Action,
    ) -> Result<String, Fail> {
        tokio::task::spawn_blocking(move || {
            let (broker, all_windows) = self.broker(from, &session)?;
            let mut connection = broker.lock().unwrap();
            if !Arc::ptr_eq(&broker, &self.broker(from, &session)?.0) {
                return Err(Fail::refused("разрешение для окна изменилось"));
            }
            let acts = !matches!(action, admin::Action::Probe);
            if all_windows && window.is_none() {
                return if !connection.is_active() {
                    Err(Fail::refused(
                        "разрешение завершилось; обнови статус на Windows",
                    ))
                } else if acts {
                    Err(Fail::refused(
                        "укажи окно явно через --window @ID; список: ferry windows",
                    ))
                } else {
                    Ok(json!({ "scope": "all", "session": session }).to_string())
                };
            }
            let _acting = acts.then(|| {
                let who = self.note(
                    from,
                    "управление разрешённым окном администратора".into(),
                    true,
                );
                super::Acting::new(&self, who)
            });
            connection.execute_on(window, action).map_err(Fail::from)
        })
        .await
        .map_err(Fail::new)?
    }
}

impl Agent {
    pub async fn admin_windows(&self, action: &str, window: Option<i64>) -> Result<Value, Fail> {
        let inner = &self.inner;
        match action {
            "show" => Ok(inner.admin_report(None)),
            "choose" => {
                let windows = admin::candidates()?;
                let selection = inner.admin_owner().map(|(owner, _)| Selection {
                    owner,
                    windows: windows.clone(),
                    until: Instant::now() + Duration::from_secs(120),
                });
                inner.admin.lock().unwrap().selection = selection;
                Ok(inner.admin_report(Some(&windows)))
            }
            "revoke" => {
                let old = std::mem::take(&mut *inner.admin.lock().unwrap());
                drop(old);
                inner.notify();
                Ok(inner.admin_report(None))
            }
            "grant" | "grant_all" => {
                let all_windows = action == "grant_all";
                let (owner, lease_until) = inner.admin_owner().ok_or_else(|| {
                    Fail::refused("сначала удалённый пульт должен занять рабочий стол")
                })?;
                let until = (!all_windows).then(|| lease_until.min(Instant::now() + MAX_GRANT));
                let request_id = ferry_core::jobs::unique_id();
                if !admin::desktop_available() {
                    return Err(Fail::refused(
                        "рабочий стол Windows заблокирован или защищён",
                    ));
                }
                let (approved, old) = {
                    let mut state = inner.admin.lock().unwrap();
                    if state.pending.is_some() {
                        return Err(Fail::refused("подтверждение уже открыто"));
                    }
                    let selection = state.selection.take();
                    let approved = if all_windows {
                        None
                    } else {
                        let selection = selection
                            .filter(|choice| choice.owner == owner && choice.until > Instant::now())
                            .ok_or_else(|| {
                                Fail::refused(
                                    "бронь изменилась или выбор истёк; выбери окно заново",
                                )
                            })?;
                        let approved = selection
                            .windows
                            .into_iter()
                            .find(|w| Some(w.id()) == window)
                            .ok_or_else(|| Fail::refused("выбери окно из списка"))?;
                        if !admin::candidates()?.iter().any(|candidate| {
                            candidate.id() == approved.id() && candidate.pid() == approved.pid()
                        }) {
                            return Err(Fail::refused(
                                "выбранное окно изменилось; выбери окно заново",
                            ));
                        }
                        Some(approved)
                    };
                    state.pending = Some(request_id.clone());
                    (approved, state.grant.take())
                };
                drop(old);
                inner.notify();
                let mut label = inner.owner_name(&owner);
                if !all_windows {
                    label = format!("{label} ({})", owner.session);
                }
                let target = approved.as_ref().map(|window| (window.id(), window.pid()));
                let broker = tokio::task::spawn_blocking(move || {
                    let mut broker = Broker::start(
                        target.map_or(0, |(id, _)| id),
                        until.map_or(Duration::ZERO, |until| {
                            until.saturating_duration_since(Instant::now())
                        }),
                        &label,
                    )?;
                    if let Some((id, pid)) = target {
                        let window: Window =
                            serde_json::from_str(&broker.execute_on(None, admin::Action::Probe)?)?;
                        if window.id() != id || window.pid() != pid {
                            return Err(std::io::Error::other("выбранное окно изменилось"));
                        }
                    }
                    Ok(broker)
                })
                .await;
                let still_ours = inner.admin_owner().is_some_and(|(current, _)| {
                    if all_windows {
                        current.from == owner.from
                    } else {
                        current == owner
                    }
                }) && admin::desktop_available()
                    && until.is_none_or(|until| until > Instant::now());
                {
                    let mut state = inner.admin.lock().unwrap();
                    if state.pending.as_deref() != Some(&request_id) {
                        return Err(Fail::refused("подтверждение отозвано"));
                    }
                    state.pending = None;
                    let broker = broker.map_err(Fail::new)??;
                    if !still_ours {
                        return Err(Fail::refused("бронь изменилась или истекла"));
                    }
                    state.grant = Some(Grant {
                        owner,
                        window: approved,
                        until,
                        broker: Arc::new(Mutex::new(broker)),
                    });
                }
                inner.notify();
                Ok(inner.admin_report(None))
            }
            _ => Err(Fail::new("неизвестное действие разрешения Windows")),
        }
    }
}
