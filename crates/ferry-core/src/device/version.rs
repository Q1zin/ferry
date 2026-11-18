pub const SHELL: &str = "0.1.18";
pub const RESULT: &str = "0.1.20";
pub const CAPABILITIES: &str = "0.1.21";
pub const DESK_NOTE: &str = "0.1.23";
pub const DURABLE_START: &str = "0.1.23";
pub const CHECK: &str = "0.1.23";
pub const DESK_AWAKE: &str = "0.1.27";

pub const FEATURES: [(&str, &str); 6] = [
    ("shell", SHELL),
    ("result", RESULT),
    ("desk-note", DESK_NOTE),
    ("durable-start", DURABLE_START),
    ("check", CHECK),
    ("desk-awake", DESK_AWAKE),
];

pub fn parse(value: &str) -> Option<[u32; 3]> {
    let mut parts = value.split('.');
    let mut next = || {
        let part = parts.next()?;
        part.bytes()
            .all(|b| b.is_ascii_digit())
            .then_some(part)?
            .parse()
            .ok()
    };
    let version = [next()?, next()?, next()?];
    parts.next().is_none().then_some(version)
}

pub fn at_least(value: &str, minimum: &str) -> bool {
    matches!((parse(value), parse(minimum)), (Some(actual), Some(minimum)) if actual >= minimum)
}
