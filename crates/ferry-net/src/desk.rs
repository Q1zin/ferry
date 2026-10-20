//! Бронь рабочего стола устройства: чьи окна и ввод сейчас пускать. Без неё два агента,
//! гоняющие тесты на одном устройстве, перемешали бы клики и текст. Кто — управляющее
//! устройство и сессия на нём (агент или терминал); любое действие с окнами само занимает
//! стол на [`HOLD`] и продлевает бронь, явная — `ferry desk <устройство> take`.

use std::time::{Duration, Instant};

use iroh::EndpointId;

use crate::proto::DeskAction;

/// Сколько держится бронь после последнего действия: между шагами агент думает.
pub const HOLD: Duration = Duration::from_secs(180);

/// Кто держит стол.
#[derive(Debug, Clone, PartialEq)]
struct Holder {
    from: EndpointId,
    session: String,
    until: Instant,
}

#[derive(Debug, Default)]
pub struct Lease(Option<Holder>);

impl Lease {
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
        if self.0.as_ref().is_some_and(|h| h.until <= now) {
            self.0 = None;
        }
        let mine = |h: &Holder| h.from == from && h.session == session;
        let who = |h: &Holder| match h.session.as_str() {
            "" => name(h.from),
            session => format!("{} ({session})", name(h.from)),
        };
        let left = |h: &Holder| h.until.saturating_duration_since(now).as_secs();
        match (action, &self.0) {
            (DeskAction::Show, None) => Ok("свободен".into()),
            (DeskAction::Show, Some(h)) => Ok(format!("занят: {} — ещё {} с", who(h), left(h))),
            (DeskAction::ForceFree, None) => Ok("свободен".into()),
            (DeskAction::ForceFree, Some(h)) => {
                let was = who(h);
                self.0 = None;
                Ok(format!("свободен — бронь {was} снята"))
            }
            (DeskAction::Free | DeskAction::Hold(_), Some(h)) if !mine(h) => Err(format!(
                "занят: {} — ещё {} с; подожди или сними чужую бронь: ferry desk <устройство> free --force",
                who(h),
                left(h)
            )),
            (DeskAction::Free, _) => {
                self.0 = None;
                Ok("свободен".into())
            }
            (DeskAction::Hold(secs), _) => {
                let holder = Holder {
                    from,
                    session: session.into(),
                    until: now + Duration::from_secs(secs.into()),
                };
                let said = format!("занят тобой — ещё {} с", left(&holder));
                self.0 = Some(holder);
                Ok(said)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(lease.apply(mac, "claude:1", hold, t0, name).is_ok());
        // Та же сессия — продлевает; другая сессия того же мака и другое устройство — отказ.
        assert!(
            lease
                .apply(mac, "claude:1", hold, t0 + Duration::from_secs(60), name)
                .is_ok()
        );
        let other = lease.apply(mac, "claude:2", hold, t0 + Duration::from_secs(61), name);
        assert!(other.unwrap_err().contains("mac (claude:1) — ещё 179 с"));
        assert!(
            lease
                .apply(win, "", hold, t0 + Duration::from_secs(61), name)
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
                .apply(win, "", hold, t0 + Duration::from_secs(300), name)
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
                .apply(mac, "claude:1", hold, t0 + Duration::from_secs(302), name)
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
                .apply(win, "", hold, t0 + Duration::from_secs(304), name)
                .is_ok()
        );
    }
}
