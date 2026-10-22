//! Ограниченная сводка JUnit из фактических testcase, без эвристик по логам.
use quick_xml::{
    Reader,
    events::{BytesStart, Event},
};
use serde::Serialize;
use std::{
    fs,
    io::{self, Read},
    path::Path,
};
const MAX_XML: usize = 16 * 1024 * 1024;

#[derive(Default, Serialize)]
pub struct Report {
    pub tests: u64,
    pub passed: u64,
    pub failed: u64,
    pub errors: u64,
    pub skipped: u64,
    pub duration_seconds: f64,
    pub failures: Vec<Case>,
    pub omitted_failures: u64,
    pub warnings: Vec<String>,
}
#[derive(Clone, Default, Serialize)]
pub struct Case {
    pub suite: Vec<String>,
    pub identity_complete: bool,
    pub name: String,
    pub class: String,
    pub file: String,
    pub line: String,
    pub duration_seconds: f64,
    pub state: String,
    pub message: String,
    pub detail: String,
}
#[derive(Default, Serialize)]
pub struct Cases {
    pub summary: Report,
    pub cases: Vec<Case>,
    pub complete: bool,
    pub omitted_cases: u64,
}

pub(crate) fn clip(text: &str, max: usize) -> String {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].into()
}
fn raw_attr(e: &BytesStart<'_>, name: &str) -> io::Result<String> {
    let mut value = String::new();
    for a in e.attributes() {
        let a = a.map_err(io::Error::other)?;
        if a.key.0 == name {
            value = a
                .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .map_err(io::Error::other)?
                .into_owned();
        }
    }
    Ok(value)
}
fn attr(e: &BytesStart<'_>, name: &str) -> io::Result<String> {
    raw_attr(e, name).map(|v| clip(&v, 1024))
}
fn case(e: &BytesStart<'_>) -> io::Result<Case> {
    let time = attr(e, "time")?;
    let duration_seconds = if time.is_empty() {
        0.0
    } else {
        let n: f64 = time.parse().map_err(io::Error::other)?;
        if !n.is_finite() || n < 0.0 {
            return Err(io::Error::other("неверная длительность testcase"));
        }
        n
    };
    let identity_complete = ["name", "classname", "file"]
        .into_iter()
        .map(|n| raw_attr(e, n))
        .collect::<io::Result<Vec<_>>>()?
        .iter()
        .all(|v| v.len() <= 1024)
        && !attr(e, "name")?.is_empty();
    Ok(Case {
        identity_complete,
        name: attr(e, "name")?,
        class: attr(e, "classname")?,
        file: attr(e, "file")?,
        line: attr(e, "line")?,
        duration_seconds,
        state: "passed".into(),
        ..Default::default()
    })
}
fn finish(report: &mut Report, case: Case) {
    report.tests += 1;
    report.duration_seconds += case.duration_seconds;
    match case.state.as_str() {
        "failed" => report.failed += 1,
        "error" => report.errors += 1,
        "skipped" => report.skipped += 1,
        _ => report.passed += 1,
    }
    if matches!(case.state.as_str(), "failed" | "error") {
        if report.failures.len() < 50 {
            report.failures.push(case);
        } else {
            report.omitted_failures += 1;
        }
    }
}
fn read_xml(path: &Path) -> io::Result<String> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take((MAX_XML + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_XML {
        return Err(io::Error::other("JUnit больше 16 МиБ"));
    }
    String::from_utf8(bytes).map_err(io::Error::other)
}
pub fn read(path: &Path) -> io::Result<Report> {
    parse(&read_xml(path)?)
}
/// A bounded index of every testcase; retain the XML artifact for unabridged details.
pub fn read_cases(path: &Path) -> io::Result<Cases> {
    parse_cases(&read_xml(path)?, true)
}
fn parse(xml: &str) -> io::Result<Report> {
    Ok(parse_cases(xml, false)?.summary)
}
fn parse_cases(xml: &str, include: bool) -> io::Result<Cases> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().expand_empty_elements = true;
    let mut report = Report::default();
    let mut cases = Vec::new();
    let mut case_bytes = 0usize;
    let mut complete = true;
    let mut omitted_cases = 0u64;
    let mut suites: Vec<(String, bool)> = Vec::new();
    let mut stack: Vec<String> = Vec::new();
    let mut current: Option<Case> = None;
    let mut root = false;
    let mut expected = None;
    loop {
        match reader.read_event().map_err(io::Error::other)? {
            Event::Start(e) => {
                let name = e.name().0.to_string();
                if stack.is_empty() {
                    if root || !matches!(name.as_str(), "testsuite" | "testsuites") {
                        return Err(io::Error::other(
                            "ожидался один корень testsuite/testsuites",
                        ));
                    }
                    root = true;
                    let n = attr(&e, "tests")?;
                    if !n.is_empty() {
                        expected = Some(n.parse::<u64>().map_err(io::Error::other)?);
                    }
                }
                if stack.len() >= 64 {
                    return Err(io::Error::other("слишком глубокий JUnit"));
                }
                if name == "testsuite" {
                    let suite = raw_attr(&e, "name")?;
                    suites.push((clip(&suite, 1024), suite.len() <= 1024));
                }
                if name == "testcase" {
                    if current.is_some() || stack.last().map(String::as_str) != Some("testsuite") {
                        return Err(io::Error::other(
                            "testcase вне testsuite или вложен в testcase",
                        ));
                    }
                    let mut c = case(&e)?;
                    c.suite = suites.iter().map(|(s, _)| s.clone()).collect();
                    c.identity_complete &= suites.iter().all(|(_, complete)| *complete);
                    current = Some(c);
                } else if matches!(name.as_str(), "failure" | "error" | "skipped")
                    && stack.last().map(String::as_str) == Some("testcase")
                {
                    let c = current
                        .as_mut()
                        .ok_or_else(|| io::Error::other("нет testcase"))?;
                    if name == "error"
                        || c.state == "passed"
                        || (name == "failure" && c.state == "skipped")
                    {
                        c.state = match name.as_str() {
                            "failure" => "failed",
                            other => other,
                        }
                        .into();
                    }
                    let message = attr(&e, "message")?;
                    if !message.is_empty() {
                        c.message = clip(
                            &format!(
                                "{}{}{}",
                                c.message,
                                if c.message.is_empty() { "" } else { "\n" },
                                message
                            ),
                            2048,
                        );
                    }
                }
                stack.push(name);
            }
            Event::End(e) => {
                if stack.pop().as_deref() != Some(e.name().0) {
                    return Err(io::Error::other("непарные теги JUnit"));
                }
                if e.name().0 == "testcase" {
                    let c = current
                        .take()
                        .ok_or_else(|| io::Error::other("нет testcase"))?;
                    if include {
                        let bytes = serde_json::to_vec(&c).map_err(io::Error::other)?.len();
                        if cases.len() < 20000
                            && case_bytes.saturating_add(bytes) <= 4 * 1024 * 1024
                        {
                            complete &= c.identity_complete;
                            case_bytes += bytes;
                            cases.push(c.clone());
                        } else {
                            omitted_cases += 1;
                            complete = false;
                        }
                    }
                    finish(&mut report, c);
                }
                if e.name().0 == "testsuite" {
                    suites.pop();
                }
            }
            Event::Text(t) => {
                if stack.is_empty() && !t.trim().is_empty() {
                    return Err(io::Error::other("текст вне корня JUnit"));
                }
                append(&mut current, &stack, &t);
            }
            Event::CData(t) => append(&mut current, &stack, &t),
            Event::GeneralRef(e) => {
                let text = if let Some(c) = e.resolve_char_ref().map_err(io::Error::other)? {
                    c.to_string()
                } else {
                    quick_xml::escape::unescape(&format!("&{};", &*e))
                        .map_err(io::Error::other)?
                        .into_owned()
                };
                append(&mut current, &stack, &text);
            }
            Event::DocType(_) => {
                return Err(io::Error::other("DTD и внешние сущности в JUnit запрещены"));
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if !root || !stack.is_empty() || current.is_some() {
        return Err(io::Error::other("JUnit пуст или оборван"));
    }
    if let Some(n) = expected
        && n != report.tests
    {
        report.warnings.push(format!(
            "в корне tests={n}, фактических testcase={}",
            report.tests
        ));
    }
    if !report.duration_seconds.is_finite() {
        return Err(io::Error::other("суммарная длительность слишком велика"));
    }
    if report.tests == 0 {
        report
            .warnings
            .push("отчёт не содержит testcase; это не доказательство успешного прогона".into());
    }
    complete &= report.warnings.is_empty() && report.tests > 0;
    Ok(Cases {
        summary: report,
        cases,
        complete,
        omitted_cases,
    })
}
fn append(current: &mut Option<Case>, stack: &[String], text: &str) {
    if stack
        .last()
        .is_some_and(|s| matches!(s.as_str(), "failure" | "error" | "skipped"))
        && let Some(c) = current
        && c.detail.len() < 4096
    {
        c.detail.push_str(&clip(text, 4096 - c.detail.len()));
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cases_keep_passed_skipped_failed_and_suite_identity() {
        let report=parse_cases(r#"<testsuites tests="3"><testsuite name="suite"><testcase name="pass" classname="c"/><testcase name="skip"><skipped/></testcase><testcase name="fail"><failure message="bad">trace</failure></testcase></testsuite></testsuites>"#,true).unwrap();
        assert!(report.complete);
        assert_eq!(report.cases.len(), 3);
        assert_eq!(
            report
                .cases
                .iter()
                .map(|c| c.state.as_str())
                .collect::<Vec<_>>(),
            vec!["passed", "skipped", "failed"]
        );
        assert_eq!(report.cases[0].suite, vec!["suite"]);
        assert_eq!(report.cases[2].detail, "trace");
        let long = "x".repeat(1025);
        let truncated = parse_cases(
            &format!("<testsuite><testcase name=\"{long}\"/></testsuite>"),
            true,
        )
        .unwrap();
        assert!(!truncated.complete);
        assert!(!truncated.cases[0].identity_complete);
        let mismatch = parse_cases(
            "<testsuite tests=\"2\"><testcase name=\"one\"/></testsuite>",
            true,
        )
        .unwrap();
        assert!(!mismatch.complete);
    }
    #[test]
    fn reads_nested_suites_cdata_entities_and_real_counts() {
        let r=parse(r#"<testsuites tests="9"><testsuite><testcase name="ok" time="1.5"/><testcase name="bad &amp; русский" file="a.rs" line="5"><failure message="expected &lt; 2"><![CDATA[stack <here>]]>&amp; end</failure></testcase><testsuite><testcase name="error"><error>oops</error></testcase><testcase name="skip"><skipped/></testcase></testsuite></testsuite></testsuites>"#).unwrap();
        assert_eq!(
            (r.tests, r.passed, r.failed, r.errors, r.skipped),
            (4, 1, 1, 1, 1)
        );
        assert_eq!(r.failures[0].name, "bad & русский");
        assert_eq!(r.failures[0].detail, "stack <here>& end");
        assert_eq!(r.duration_seconds, 1.5);
        assert_eq!(r.warnings.len(), 1);
    }
    #[test]
    fn rejects_bad_reports_and_bounds_failure_output() {
        for s in [
            "",
            "<html/>",
            "<testsuite>",
            "<testsuite/><testsuite/>",
            "<!DOCTYPE testsuite><testsuite/>",
            "<testsuite><testcase time=\"NaN\"/></testsuite>",
        ] {
            assert!(parse(s).is_err(), "{s}");
        }
        let s = format!(
            "<testsuite>{}</testsuite>",
            "<testcase><failure>boom</failure></testcase>".repeat(55)
        );
        let r = parse(&s).unwrap();
        assert_eq!(r.failed, 55);
        assert_eq!(r.failures.len(), 50);
        assert_eq!(r.omitted_failures, 5);
    }
}
