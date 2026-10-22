//! One guarded action, followed by an explicit Accessibility assertion.
use super::{
    elements::{self, Element, Locator, Tree},
    observe, *,
};
use serde::{Deserialize, Serialize};
use std::{
    path::Path,
    time::{Duration, Instant},
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub window: Window,
    pub captured_at: u64,
    pub elements: Vec<Element>,
    pub complete: bool,
    pub changed_during_capture: bool,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ActionV2 {
    Click(Locator),
    Key(String),
    Type(String),
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Property {
    Exists,
    Absent,
    Value,
    Enabled,
    Focused,
    Checked,
    Selected,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Assertion {
    pub locator: Locator,
    pub property: Property,
    pub value: Option<String>,
    pub state: Option<bool>,
}
impl Assertion {
    pub fn validate(&self) -> io::Result<()> {
        self.locator.validate()?;
        let valid = match self.property {
            Property::Value => {
                self.value.as_ref().is_some_and(|v| v.len() <= 512) && self.state.is_none()
            }
            Property::Exists | Property::Absent => self.value.is_none() && self.state.is_none(),
            _ => self.value.is_none() && self.state.is_some(),
        };
        if !valid {
            return Err(io::Error::other(
                "для value нужен value, для состояния — state; exists/absent без значения",
            ));
        }
        Ok(())
    }
    pub fn evaluate(&self, tree: &Tree) -> io::Result<bool> {
        if !tree.complete {
            return Err(io::Error::other(
                "неполное Accessibility-дерево не подтверждает условие",
            ));
        }
        let hits = self.locator.hits(&tree.elements);
        if self.property == Property::Absent {
            return Ok(hits.is_empty());
        }
        let e = match hits.as_slice() {
            [] => return Ok(false),
            [e] => *e,
            _ => return Err(io::Error::other("ожидаемый элемент неоднозначен")),
        };
        let state = match self.property {
            Property::Exists => return Ok(true),
            Property::Absent => unreachable!(),
            Property::Value => {
                if e.value_truncated {
                    return Err(io::Error::other(
                        "значение длиннее 512 байт; точная проверка недоступна",
                    ));
                }
                if e.protected {
                    return Err(io::Error::other("значение защищённого поля недоступно"));
                }
                return e
                    .value
                    .as_ref()
                    .map(|v| Some(v) == self.value.as_ref())
                    .ok_or_else(|| {
                        io::Error::other("приложение не предоставляет значение элемента")
                    });
            }
            Property::Enabled => e.enabled,
            Property::Focused => e.focused,
            Property::Checked => e.checked,
            Property::Selected => e.selected,
        };
        state
            .map(|s| Some(s) == self.state)
            .ok_or_else(|| io::Error::other("приложение не предоставляет запрошенное состояние"))
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RequestV2 {
    pub snapshot: Snapshot,
    pub action: ActionV2,
    pub target: Option<Locator>,
    pub expect: Option<Assertion>,
    /// Exact title of a newly created window of the same process.
    pub new_window: Option<String>,
    pub allow_already: bool,
    pub timeout: u32,
    pub dry_run: bool,
    pub screenshot: bool,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReportV2 {
    pub performed: bool,
    pub verified: bool,
    pub dry_run: bool,
    pub transition_required: bool,
    pub error: Option<String>,
    pub observation: Option<observe::ObservationV2>,
}
fn validate(r: &RequestV2) -> io::Result<()> {
    if serde_json::to_vec(&r.snapshot)
        .map_err(io::Error::other)?
        .len()
        > 2 * 1024 * 1024
    {
        return Err(io::Error::other("наблюдение больше 2 МиБ"));
    }
    if r.timeout > 30
        || r.snapshot.elements.len() > 500
        || r.snapshot.elements.is_empty()
        || !r.snapshot.complete
        || r.snapshot.changed_during_capture
    {
        return Err(io::Error::other(
            "нужно полное стабильное структурированное наблюдение и timeout 0..30",
        ));
    }
    if crate::now().saturating_sub(r.snapshot.captured_at) > 120
        || r.snapshot.captured_at > crate::now() + 5
    {
        return Err(io::Error::other("наблюдение устарело; повтори observe"));
    }
    if r.expect.is_none() && r.new_window.is_none() {
        return Err(io::Error::other(
            "нужно условие результата или ожидаемое новое окно",
        ));
    }
    if let Some(expect) = &r.expect {
        expect.validate()?;
    }
    if let Some(target) = &r.target {
        target.validate()?;
    }
    if r.new_window
        .as_ref()
        .is_some_and(|s| s.is_empty() || s.len() > 512)
    {
        return Err(io::Error::other("неверное имя нового окна"));
    }
    match &r.action {
        ActionV2::Click(locator) => locator.validate()?,
        ActionV2::Key(s) | ActionV2::Type(s) => {
            if s.is_empty() || s.len() > 4096 {
                return Err(io::Error::other("действие должно содержать 1..4096 байт"));
            }
        }
    }
    if matches!(r.action, ActionV2::Type(_)) && r.target.is_none() {
        return Err(io::Error::other(
            "для type нужен --target с полем, уже имеющим фокус",
        ));
    }
    Ok(())
}
fn before_condition(request: &RequestV2, tree: &Tree) -> io::Result<()> {
    if request.new_window.is_none()
        && let Some(expect) = &request.expect
        && expect.evaluate(tree)?
        && !request.allow_already
    {
        return Err(io::Error::other(
            "условие уже выполнено до действия; выбери проверяемое изменение или явно --allow-already",
        ));
    }
    Ok(())
}

fn tree(window: &Window) -> io::Result<Tree> {
    let tree = os::structured(&format!("@{}", window.id))?;
    if !tree.complete {
        return Err(io::Error::other("неполное дерево; действие отменено"));
    }
    Ok(tree)
}
fn same_element(a: &Element, b: &Element) -> bool {
    let mut a = a.clone();
    let mut b = b.clone();
    a.index = 0;
    b.index = 0;
    a.parent = None;
    b.parent = None;
    a == b
}
fn guarded_target<'a>(
    locator: &Locator,
    baseline: &[Element],
    current: &'a [Element],
    focus: bool,
) -> io::Result<&'a Element> {
    let old = locator.unique(baseline)?;
    let now = locator.unique(current)?;
    let old_parents = elements::ancestors(old, baseline);
    let new_parents = elements::ancestors(now, current);
    if [old, now]
        .iter()
        .any(|e| e.name_truncated || e.id_truncated || e.value_truncated)
        || old_parents
            .iter()
            .chain(new_parents.iter())
            .any(|e| e.name_truncated || e.id_truncated)
    {
        return Err(io::Error::other(
            "поля цели или контейнера обрезаны; уточни область или используй доступный элемент",
        ));
    }
    let identity = |e: &&Element| (e.role.clone(), e.name.clone(), e.id.clone(), e.rect);
    if !same_element(old, now)
        || old_parents.iter().map(identity).collect::<Vec<_>>()
            != new_parents.iter().map(identity).collect::<Vec<_>>()
    {
        return Err(io::Error::other(
            "целевой элемент или его контейнер изменились; повтори observe",
        ));
    }
    if now.enabled != Some(true) || (focus && now.focused != Some(true)) {
        return Err(io::Error::other(
            "цель недоступна или не имеет фокуса для клавиатурного ввода",
        ));
    }
    Ok(now)
}
fn guarded_focus(baseline: &[Element], current: &[Element]) -> io::Result<()> {
    let old: Vec<_> = baseline
        .iter()
        .filter(|e| e.focused == Some(true))
        .collect();
    let now: Vec<_> = current.iter().filter(|e| e.focused == Some(true)).collect();
    if old.len() != now.len() || old.iter().zip(now.iter()).any(|(a, b)| !same_element(a, b)) {
        return Err(io::Error::other("элемент с фокусом изменился"));
    }
    Ok(())
}
fn point(e: &Element, w: &Window) -> io::Result<(i32, i32)> {
    let (Some((x, y, width, height)), Some((wx, wy, ww, wh))) = (e.rect, w.rect) else {
        return Err(io::Error::other("нет координат цели"));
    };
    let (cx, cy) = (x.saturating_add(width / 2), y.saturating_add(height / 2));
    if width <= 0
        || height <= 0
        || cx < wx
        || cy < wy
        || cx >= wx.saturating_add(ww)
        || cy >= wy.saturating_add(wh)
    {
        return Err(io::Error::other("центр элемента вне окна"));
    }
    Ok((
        i32::try_from(cx).map_err(io::Error::other)?,
        i32::try_from(cy).map_err(io::Error::other)?,
    ))
}
fn post_window(r: &RequestV2, before: &[Window], windows: &[Window]) -> io::Result<Option<Window>> {
    let Some(focused) = windows.iter().find(|w| w.focused) else {
        return Err(io::Error::other("после действия нет окна с фокусом"));
    };
    if let Some(title) = &r.new_window {
        if focused.id == r.snapshot.window.id && focused.pid == r.snapshot.window.pid {
            return Ok(None);
        }
        if focused.pid == r.snapshot.window.pid
            && &focused.title == title
            && focused.rect.is_some()
            && !before.iter().any(|w| w.id == focused.id)
        {
            return Ok(Some(focused.clone()));
        }
    } else if focused.id == r.snapshot.window.id && focused.pid == r.snapshot.window.pid {
        return Ok(Some(focused.clone()));
    }
    Err(io::Error::other(
        "фокус перешёл в неожиданное окно; действие не повторять",
    ))
}
fn expected_result_window(request: &RequestV2, window: &Window) -> bool {
    window.pid == request.snapshot.window.pid
        && request
            .new_window
            .as_ref()
            .is_none_or(|title| &window.title == title && window.id != request.snapshot.window.id)
}
pub fn execute_v2(request: RequestV2, root: &Path) -> io::Result<ReportV2> {
    validate(&request)?;
    let _guard = INPUT_LOCK
        .lock()
        .map_err(|_| io::Error::other("ввод прервался"))?;
    validate(&request)?;
    let mut report = ReportV2 {
        performed: false,
        verified: false,
        dry_run: request.dry_run,
        transition_required: !request.allow_already,
        error: None,
        observation: None,
    };
    let mut selected = request.snapshot.window.clone();
    let result = (|| -> io::Result<()> {
        #[cfg(target_os = "macos")]
        if locked() {
            return Err(io::Error::other("экран заблокирован"));
        }
        let before_windows = os::windows()?;
        super::act::stable(&request.snapshot.window, &before_windows)?;
        let current = tree(&request.snapshot.window)?;
        let locator = match &request.action {
            ActionV2::Click(l) => Some(l),
            _ => request.target.as_ref(),
        };
        let target = if let Some(locator) = locator {
            Some(guarded_target(
                locator,
                &request.snapshot.elements,
                &current.elements,
                !matches!(request.action, ActionV2::Click(_)),
            )?)
        } else {
            guarded_focus(&request.snapshot.elements, &current.elements)?;
            None
        };
        let click = if matches!(request.action, ActionV2::Click(_)) {
            Some(point(target.unwrap(), &request.snapshot.window)?)
        } else {
            None
        };
        before_condition(&request, &current)?;
        let reread = tree(&request.snapshot.window)?;
        if let Some(locator) = locator {
            guarded_target(
                locator,
                &current.elements,
                &reread.elements,
                !matches!(request.action, ActionV2::Click(_)),
            )?;
        } else {
            guarded_focus(&current.elements, &reread.elements)?;
        }
        before_condition(&request, &reread)?;
        super::act::stable(&request.snapshot.window, &os::windows()?)?;
        if request.dry_run {
            return Ok(());
        }
        report.performed = true;
        match &request.action {
            ActionV2::Click(_) => {
                let (x, y) = click.unwrap();
                os::pointer(x, y, &[MOUSE_LEFT_DOWN, MOUSE_LEFT_UP], "")?;
            }
            ActionV2::Key(combo) => {
                os::keys(&[combo])?;
            }
            ActionV2::Type(text) => {
                os::type_text(text)?;
            }
        }
        let deadline = Instant::now() + Duration::from_secs(request.timeout.into());
        loop {
            if let Some(window) = post_window(&request, &before_windows, &os::windows()?)? {
                selected = window;
                let current = tree(&selected)?;
                if request
                    .expect
                    .as_ref()
                    .map(|e| e.evaluate(&current))
                    .transpose()?
                    .unwrap_or(true)
                {
                    report.verified = true;
                    break;
                }
            }
            if Instant::now() >= deadline {
                return Err(io::Error::other(
                    "условие результата не выполнено; действие не повторять автоматически",
                ));
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        Ok(())
    })();
    if let Err(e) = result {
        report.error = Some(e.to_string());
    }
    // Capture only the selected, owned window, even after an unrelated focus switch.
    let final_windows = os::windows();
    if let Err(e) = &final_windows {
        report.error.get_or_insert(e.to_string());
    }
    if final_windows.is_ok_and(|windows| {
        windows
            .iter()
            .any(|w| w.id == selected.id && w.pid == selected.pid)
    }) {
        let query = request
            .expect
            .as_ref()
            .filter(|e| e.property != Property::Absent)
            .map(|e| e.locator.clone());
        match observe::capture_v2(
            observe::RequestV2 {
                window: format!("@{}", selected.id),
                ocr: false,
                screenshot: request.screenshot,
                query,
                subtree: false,
            },
            root,
        ) {
            Ok(observation) => {
                if report.verified {
                    let stable = !observation.changed_during_capture && observation.window.focused;
                    // Absent requires the complete fresh tree, not a filtered result.
                    let confirmed = tree(&observation.window)
                        .and_then(|t| {
                            request
                                .expect
                                .as_ref()
                                .map(|e| e.evaluate(&t))
                                .transpose()
                                .map(|v| v.unwrap_or(true))
                        })
                        .unwrap_or(false);
                    let focused = os::windows().is_ok_and(|windows| {
                        windows.iter().find(|w| w.focused) == Some(&observation.window)
                    });
                    let expected_window = expected_result_window(&request, &observation.window);
                    report.verified = stable && confirmed && focused && expected_window;
                }
                report.observation = Some(observation);
            }
            Err(e) => {
                report.verified = false;
                report.error.get_or_insert(e.to_string());
            }
        }
    } else {
        report.verified = false;
        report
            .error
            .get_or_insert("своё окно недоступно для итогового наблюдения".into());
    }
    if report.performed && !report.verified {
        report
            .error
            .get_or_insert("результат не подтверждён; действие не повторять автоматически".into());
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn element() -> Element {
        Element {
            index: 1,
            parent: None,
            role: "CheckBox".into(),
            name: "Enabled".into(),
            id: "toggle".into(),
            rect: Some((0, 0, 20, 20)),
            enabled: Some(true),
            focused: Some(true),
            checked: Some(false),
            selected: None,
            value: None,
            protected: false,
            name_truncated: false,
            id_truncated: false,
            value_truncated: false,
        }
    }
    #[test]
    fn assertions_require_exposed_state_and_complete_tree() {
        let mut tree = Tree {
            elements: vec![element()],
            complete: true,
        };
        let mut a = Assertion {
            locator: Locator {
                id: Some("toggle".into()),
                ..Default::default()
            },
            property: Property::Checked,
            value: None,
            state: Some(true),
        };
        a.validate().unwrap();
        assert!(!a.evaluate(&tree).unwrap());
        tree.elements[0].checked = Some(true);
        assert!(a.evaluate(&tree).unwrap());
        tree.elements[0].checked = None;
        assert!(a.evaluate(&tree).is_err());
        a.property = Property::Value;
        a.value = Some("prefix".into());
        a.state = None;
        tree.elements[0].value = Some("prefix".into());
        tree.elements[0].value_truncated = true;
        assert!(a.evaluate(&tree).is_err());
        a.property = Property::Absent;
        a.state = None;
        a.value = None;
        tree.elements.clear();
        assert!(a.evaluate(&tree).unwrap());
        tree.complete = false;
        assert!(a.evaluate(&tree).is_err());
    }
    #[test]
    fn guards_target_but_allows_unrelated_change_and_refuses_motion() {
        let baseline = vec![element()];
        let mut now = baseline.clone();
        let l = Locator {
            id: Some("toggle".into()),
            ..Default::default()
        };
        let mut unrelated = element();
        unrelated.id = "unrelated".into();
        unrelated.index = 2;
        unrelated.value_truncated = true;
        now.push(unrelated);
        assert!(guarded_target(&l, &baseline, &now, true).is_ok());
        now[0].rect = Some((20, 0, 20, 20));
        assert!(guarded_target(&l, &baseline, &now, true).is_err());
        now[0] = element();
        now[0].focused = Some(false);
        assert!(guarded_target(&l, &baseline, &now, true).is_err());
    }
    #[test]
    fn new_window_must_be_new_exact_title_and_same_process() {
        let w = Window {
            id: 1,
            pid: 42,
            process: "fixture".into(),
            title: "Main".into(),
            rect: Some((0, 0, 100, 100)),
            focused: true,
        };
        let r = RequestV2 {
            snapshot: Snapshot {
                window: w.clone(),
                captured_at: crate::now(),
                elements: vec![element()],
                complete: true,
                changed_during_capture: false,
            },
            action: ActionV2::Key("enter".into()),
            target: None,
            expect: None,
            new_window: Some("Saved".into()),
            allow_already: false,
            timeout: 1,
            dry_run: true,
            screenshot: false,
        };
        let mut next = w.clone();
        next.id = 2;
        next.title = "Saved".into();
        assert!(
            post_window(&r, std::slice::from_ref(&w), std::slice::from_ref(&next))
                .unwrap()
                .is_some()
        );
        assert!(post_window(&r, &[w.clone(), next.clone()], std::slice::from_ref(&next)).is_err());
        assert!(expected_result_window(&r, &next));
        next.title = "Changed after capture".into();
        assert!(!expected_result_window(&r, &next));
        next.title = "Saved".into();
        next.pid = 43;
        assert!(post_window(&r, &[w], &[next]).is_err());
        let mut r = r;
        r.new_window = None;
        r.expect = Some(Assertion {
            locator: Locator {
                id: Some("toggle".into()),
                ..Default::default()
            },
            property: Property::Checked,
            value: None,
            state: Some(false),
        });
        let current = Tree {
            elements: vec![element()],
            complete: true,
        };
        assert!(before_condition(&r, &current).is_err());
        r.allow_already = true;
        assert!(before_condition(&r, &current).is_ok());
        r.allow_already = false;
        r.expect.as_mut().unwrap().state = Some(true);
        assert!(before_condition(&r, &current).is_ok());
    }
}
