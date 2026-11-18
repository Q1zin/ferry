//! Бронь рабочего стола устройства: чьи окна и ввод сейчас пускать. Без неё два агента,
//! гоняющие тесты на одном устройстве, перемешали бы клики и текст. Кто — управляющее
//! устройство и сессия на нём (агент или терминал); любое действие с окнами само занимает
//! стол на [`HOLD`] и продлевает бронь, явная — `ferry desk <устройство> take`.

use std::time::{Duration, Instant};

use ferry_core::awake::Inhibitor;
use iroh::EndpointId;

use crate::proto::DeskAction;

/// Сколько держится бронь после последнего действия: между шагами агент думает.
pub const HOLD: Duration = Duration::from_secs(180);
pub const MAX_AWAKE_SECONDS: u32 = 12 * 60 * 60;

/// Кто держит стол.
#[derive(Debug, Clone, PartialEq)]
struct Holder {
    from: EndpointId,
    session: String,
    until: Instant,
    note: Option<String>,
    #[cfg(windows)]
    since: Instant,
}

#[cfg(windows)]
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Owner {
    pub from: EndpointId,
    pub session: String,
    since: Instant,
}

impl Holder {
    fn who(&self, name: impl Fn(EndpointId) -> String) -> String {
        match self.session.as_str() {
            "" => name(self.from),
            session => format!("{} ({session})", name(self.from)),
        }
    }
}

#[derive(Default)]
pub struct Lease {
    holder: Option<Holder>,
    awake: Option<(Inhibitor, Instant)>,
}

impl Lease {
    #[cfg(windows)]
    pub(crate) fn owner(&self, now: Instant) -> Option<(Owner, Instant)> {
        let holder = self.holder.as_ref().filter(|holder| holder.until > now)?;
        Some((
            Owner {
                from: holder.from,
                session: holder.session.clone(),
                since: holder.since,
            },
            holder.until,
        ))
    }

    pub fn note(&self, now: Instant) -> Option<String> {
        self.holder
            .as_ref()
            .filter(|h| h.until > now)
            .and_then(|h| h.note.clone())
            .or_else(|| {
                self.awake
                    .as_ref()
                    .filter(|(_, until)| *until > now)
                    .map(|_| "Бронь без сна".into())
            })
    }

    fn clear(&mut self) {
        self.holder = None;
        self.awake = None;
    }

    pub fn maintain(&mut self, now: Instant, desktop_allowed: bool) -> bool {
        if self.holder.as_ref().is_some_and(|h| h.until <= now) {
            self.clear();
            return true;
        }
        if !desktop_allowed || self.awake.as_ref().is_some_and(|(_, until)| *until <= now) {
            return self.awake.take().is_some();
        }
        false
    }

    pub fn forget(&mut self, peer: EndpointId) {
        if self.holder.as_ref().is_some_and(|h| h.from == peer) {
            self.clear();
        }
    }

    /// Применить действие `session` с устройства `from`. Ответ — для человека; отказ — если стол
    /// держит другой. `name` — как назвать устройство в ответе.
    pub fn apply(
        &mut self,
        from: EndpointId,
        session: &str,
        action: DeskAction,
        now: Instant,
        name: impl Fn(EndpointId) -> String,
    ) -> Result<String, String> {
        let (action, note, stay_awake) = match action {
            DeskAction::HoldNote { seconds, note } => {
                (DeskAction::Hold(seconds), Some(note), false)
            }
            DeskAction::HoldAwake { seconds, note } => {
                if !(1..=MAX_AWAKE_SECONDS).contains(&seconds) {
                    return Err("бронь без сна: срок от 1 секунды до 12 часов".into());
                }
                (DeskAction::Hold(seconds), note, true)
            }
            action => (action, None, false),
        };
        if note
            .as_ref()
            .is_some_and(|n| n.chars().count() > 160 || n.chars().any(char::is_control))
        {
            return Err("заметка: до 160 символов, без управляющих символов".into());
        }
        self.maintain(now, true);
        let mine = |h: &Holder| h.from == from && h.session == session;
        let who = |h: &Holder| h.who(&name);
        let left = |h: &Holder| h.until.saturating_duration_since(now).as_secs();
        match (action, &self.holder) {
            (DeskAction::HoldNote { .. } | DeskAction::HoldAwake { .. }, _) => unreachable!(),
            (DeskAction::ShowReport, holder) => Ok(serde_json::json!({
                "state": if holder.is_some() { "busy" } else { "free" },
                "session": session,
                "owner": holder.as_ref().map(|h| serde_json::json!({
                    "device": name(h.from),
                    "device_id": h.from.to_string(),
                    "session": h.session,
                })),
                "owned": holder.as_ref().is_some_and(mine),
                "remaining_seconds": holder.as_ref().map(left),
                "note": holder.as_ref().and_then(|h| h.note.as_ref()),
                "keep_awake": self.awake.is_some(),
                "awake_remaining_seconds": self.awake.as_ref().map(|(_, until)| until.saturating_duration_since(now).as_secs())
            })
            .to_string()),
            (DeskAction::Show, None) => Ok("свободен".into()),
            (DeskAction::Show, Some(h)) => Ok(format!("занят: {} — ещё {} с", who(h), left(h))),
            (DeskAction::ForceFree, None) => Ok("свободен".into()),
            (DeskAction::ForceFree, Some(h)) => {
                let was = who(h);
                self.clear();
                Ok(format!("свободен — бронь {was} снята"))
            }
            (DeskAction::ReleaseControl | DeskAction::ReleaseOwned, Some(h)) if !mine(h) => Ok("своей брони нет".into()),
            (DeskAction::ReleaseControl, _) if self.awake.is_some() => Ok("бронь без сна сохранена".into()),
            (DeskAction::Free | DeskAction::Hold(_), Some(h)) if !mine(h) => Err(format!(
                "занят: {} — ещё {} с; подожди или выбери другое устройство: ferry select --desktop",
                who(h),
                left(h)
            )),
            (DeskAction::Free | DeskAction::ReleaseOwned | DeskAction::ReleaseControl, _) => {
                self.clear();
                Ok("свободен".into())
            }
            (DeskAction::Hold(secs), _) => {
                if stay_awake {
                    let until = now + Duration::from_secs(secs.into());
                    if let Some((_, current)) = &mut self.awake {
                        *current = (*current).max(until);
                    } else {
                        self.awake = Some((Inhibitor::acquire().map_err(|e| format!("удержание питания: {e}"))?, until));
                    }
                }
                let holder = Holder {
                    from,
                    session: session.into(),
                    #[cfg(windows)]
                    since: self.holder.as_ref().filter(|h| mine(h)).map_or(now, |h| h.since),
                    until: self
                        .holder
                        .as_ref()
                        .filter(|h| mine(h))
                        .map_or(now + Duration::from_secs(secs.into()), |h| {
                            h.until.max(now + Duration::from_secs(secs.into()))
                        }),
                    note: match note {
                        Some(note) => (!note.is_empty()).then_some(note),
                        None => self.holder.as_ref().filter(|h| mine(h)).and_then(|h| h.note.clone()),
                    },
                };
                let mut said = format!("занят тобой — ещё {} с", left(&holder));
                if stay_awake {
                    said.push_str(", удержание питания включено");
                }
                self.holder = Some(holder);
                Ok(said)
            }
        }
    }
}

/// Сколько рамка управления горит после конца последнего действия.
pub const GRACE: Duration = Duration::from_secs(10);

/// Кто сейчас действует на рабочем столе — для рамки управления. Она горит, пока действие идёт
/// (ввод, снимок, `run --desk` до конца команды), и ещё [`GRACE`] после; бронь на неё не влияет —
/// забытый `take` не держит рамку.
#[derive(Debug, Default)]
pub struct Acting {
    now: Vec<String>,
    last: Option<(Instant, String)>,
}

impl Acting {
    pub fn start(&mut self, who: String) {
        self.now.push(who);
    }

    pub fn finish(&mut self, who: &str, at: Instant) {
        if let Some(i) = self.now.iter().position(|w| w == who) {
            self.last = Some((at, self.now.remove(i)));
        }
    }

    pub fn who(&self, now: Instant) -> Option<String> {
        let recent = self.last.as_ref().filter(|(at, _)| now < *at + GRACE);
        self.now.last().or(recent.map(|(_, who)| who)).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_distinguishes_current_session_from_lease_owner() {
        let from = iroh::SecretKey::generate().public();
        let now = Instant::now();
        let mut desk = Lease::default();
        let name = |_| "console".to_string();
        desk.apply(from, "agent:first", DeskAction::Hold(60), now, name)
            .unwrap();
        let report = desk
            .apply(from, "agent:second", DeskAction::ShowReport, now, name)
            .unwrap();
        let report: serde_json::Value = serde_json::from_str(&report).unwrap();
        assert_eq!(report["session"], "agent:second");
        assert_eq!(report["owner"]["session"], "agent:first");
        assert_eq!(report["owned"], false);
        assert_eq!(report["remaining_seconds"], 60);
    }

    #[test]
    fn note_survives_renewal_without_shortening_explicit_lease() {
        let from = iroh::SecretKey::generate().public();
        let now = Instant::now();
        let mut lease = Lease::default();
        lease
            .apply(
                from,
                "agent:one",
                DeskAction::HoldNote {
                    seconds: 600,
                    note: "checking settings".into(),
                },
                now,
                |_| "test".into(),
            )
            .unwrap();
        lease
            .apply(from, "agent:one", DeskAction::Hold(180), now, |_| {
                "test".into()
            })
            .unwrap();
        assert_eq!(
            lease.note(now + Duration::from_secs(500)).as_deref(),
            Some("checking settings")
        );
        assert_eq!(lease.note(now + Duration::from_secs(601)), None);
        lease
            .apply(from, "agent:one", DeskAction::Free, now, |_| "test".into())
            .unwrap();
        assert_eq!(lease.note(now), None);
    }

    #[test]
    fn release_owned_preserves_a_replacement_lease() {
        let from = iroh::SecretKey::generate().public();
        let now = Instant::now();
        let mut lease = Lease::default();
        lease
            .apply(from, "agent:new", DeskAction::Hold(300), now, |_| {
                "test".into()
            })
            .unwrap();
        lease
            .apply(from, "agent:old", DeskAction::ReleaseOwned, now, |_| {
                "test".into()
            })
            .unwrap();
        assert!(
            lease
                .apply(from, "agent:other", DeskAction::Hold(10), now, |_| "test"
                    .into())
                .is_err()
        );
        lease
            .apply(from, "agent:new", DeskAction::ReleaseOwned, now, |_| {
                "test".into()
            })
            .unwrap();
        lease
            .apply(from, "agent:other", DeskAction::Hold(10), now, |_| {
                "test".into()
            })
            .unwrap();
    }
    #[test]
    fn one_holder_at_a_time() {
        let (mac, win) = (
            iroh::SecretKey::generate().public(),
            iroh::SecretKey::generate().public(),
        );
        let name = |id: EndpointId| {
            if id == mac {
                "mac".into()
            } else {
                "win".into()
            }
        };
        let t0 = Instant::now();
        let mut lease = Lease::default();
        let hold = DeskAction::Hold(180);

        assert_eq!(
            lease.apply(mac, "claude:1", DeskAction::Show, t0, name),
            Ok("свободен".into())
        );
        assert!(lease.apply(mac, "claude:1", hold.clone(), t0, name).is_ok());
        // Та же сессия — продлевает; другая сессия того же мака и другое устройство — отказ.
        assert!(
            lease
                .apply(
                    mac,
                    "claude:1",
                    hold.clone(),
                    t0 + Duration::from_secs(60),
                    name
                )
                .is_ok()
        );
        let other = lease.apply(
            mac,
            "claude:2",
            hold.clone(),
            t0 + Duration::from_secs(61),
            name,
        );
        assert!(other.unwrap_err().contains("mac (claude:1) — ещё 179 с"));
        assert!(
            lease
                .apply(win, "", hold.clone(), t0 + Duration::from_secs(61), name)
                .is_err()
        );
        assert!(
            lease.apply(win, "", DeskAction::Free, t0, name).is_err(),
            "чужую — только --force"
        );
        assert_eq!(
            lease.apply(
                win,
                "",
                DeskAction::Show,
                t0 + Duration::from_secs(61),
                name
            ),
            Ok("занят: mac (claude:1) — ещё 179 с".into())
        );

        // Бронь истекла — стол свободен для любого.
        assert!(
            lease
                .apply(win, "", hold.clone(), t0 + Duration::from_secs(300), name)
                .is_ok()
        );
        // Снять чужую — только силой; своя снимается просто.
        let forced = lease.apply(
            mac,
            "claude:1",
            DeskAction::ForceFree,
            t0 + Duration::from_secs(301),
            name,
        );
        assert_eq!(forced, Ok("свободен — бронь win снята".into()));
        assert!(
            lease
                .apply(
                    mac,
                    "claude:1",
                    hold.clone(),
                    t0 + Duration::from_secs(302),
                    name
                )
                .is_ok()
        );
        assert!(
            lease
                .apply(
                    mac,
                    "claude:1",
                    DeskAction::Free,
                    t0 + Duration::from_secs(303),
                    name
                )
                .is_ok()
        );
        assert!(
            lease
                .apply(win, "", hold.clone(), t0 + Duration::from_secs(304), name)
                .is_ok()
        );
    }

    #[test]
    fn frame_lasts_while_acting_and_a_bit_after() {
        let t0 = Instant::now();
        let mut acting = Acting::default();
        assert_eq!(acting.who(t0), None);
        acting.start("mac".into());
        acting.start("win".into());
        // Пока действие идёт — сколько угодно долго.
        assert_eq!(
            acting.who(t0 + Duration::from_secs(600)).as_deref(),
            Some("win")
        );
        acting.finish("win", t0 + Duration::from_secs(600));
        assert_eq!(
            acting.who(t0 + Duration::from_secs(700)).as_deref(),
            Some("mac")
        );
        acting.finish("mac", t0 + Duration::from_secs(700));
        assert_eq!(
            acting.who(t0 + Duration::from_secs(709)).as_deref(),
            Some("mac")
        );
        assert_eq!(acting.who(t0 + Duration::from_secs(710)), None);
    }
}
