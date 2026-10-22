use super::*;
use journal::Operation;
use std::{collections::BTreeMap, fs, path::Path, time::Instant};

fn summary(report: &Value) -> Value {
    let rows = report["results"].as_array().cloned().unwrap_or_default();
    let sum = |key: &str| {
        rows.iter()
            .filter_map(|r| r["tests"][key].as_u64())
            .sum::<u64>()
    };
    json!({"id":report["id"],"profile":report["profile"],"created":report["created"],
        "finished_at":report["finished_at"],"duration_seconds":report["duration_seconds"],"state":report["state"],
        "worker_active":report["worker_active"],"total":report["total"],"succeeded":report["succeeded"],
        "devices":rows.iter().map(|r|json!({"device":r["device"],"state":r["state"]})).collect::<Vec<_>>(),
        "summary":{"tests":sum("tests"),"passed":sum("passed"),"failed":sum("failed"),"errors":sum("errors"),"skipped":sum("skipped"),
        "artifacts":rows.iter().filter_map(|r|r["artifacts"].as_array()).map(Vec::len).sum::<usize>()}})
}

fn listing(parent: &Path, limit: usize, before: Option<&str>) -> Result<Value, Fail> {
    let before = before
        .map(|s| {
            let (stamp, id) = s
                .split_once(':')
                .ok_or_else(|| Fail::new("cursor: CREATED:ID"))?;
            journal::valid_id(id)?;
            Ok::<_, Fail>((stamp.parse::<u64>().map_err(Fail::new)?, id.to_string()))
        })
        .transpose()?;
    let mut entries = Vec::new();
    let mut errors = Vec::new();
    let mut scan_truncated = false;
    let started = Instant::now();
    if parent.exists() {
        for (index, entry) in fs::read_dir(parent)?.enumerate() {
            if index >= 10000 || started.elapsed().as_secs() > 5 {
                scan_truncated = true;
                break;
            }
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let path = entry.path();
            match journal::read::<Operation>(&path.join("operation.json")) {
                Ok(op)
                    if before
                        .as_ref()
                        .is_none_or(|b| (op.created, &op.id) < (b.0, &b.1)) =>
                {
                    entries.push((op.created, op.id, path))
                }
                Ok(_) => {}
                Err(e) => {
                    if errors.len() < 50 {
                        errors.push(format!("{}: {e}", entry.file_name().to_string_lossy()));
                    }
                }
            }
        }
    }
    entries.sort_by(|a, b| (b.0, &b.1).cmp(&(a.0, &a.1)));
    let more = entries.len() > limit;
    entries.truncate(limit);
    let next_cursor = more
        .then(|| entries.last().map(|e| format!("{}:{}", e.0, e.1)))
        .flatten();
    let mut checks = Vec::new();
    for (_, id, path) in entries {
        match journal::report(&path) {
            Ok(r) => checks.push(summary(&r)),
            Err(e) => errors.push(format!("{id}: {e}")),
        }
    }
    Ok(
        json!({"schema_version":1,"checks":checks,"next_cursor":next_cursor,"scan_truncated":scan_truncated,"errors":errors}),
    )
}

pub(super) fn list(args: &[&str]) -> io::Result<i32> {
    let (mut limit, mut before, mut rest) = (50usize, None, args);
    while !rest.is_empty() {
        match rest {
            ["--limit", n, tail @ ..] => {
                limit = n.parse().map_err(io::Error::other)?;
                rest = tail;
            }
            ["--before", cursor, tail @ ..] => {
                before = Some(*cursor);
                rest = tail;
            }
            ["--json", tail @ ..] => rest = tail,
            _ => return crate::usage(),
        }
    }
    finish((|| {
        if !(1..=200).contains(&limit) {
            return Err(Fail::new("limit: 1..200"));
        }
        let value = listing(&ferry_core::ferry_home()?.join("checks"), limit, before)?;
        println!("{value}");
        Ok(0)
    })())
}

pub(super) fn capture_cases(
    root: &Path,
    index: usize,
    device: &Option<String>,
    phase: &journal::Phase,
    artifact: &Value,
) -> Result<Value, Fail> {
    if artifact["bytes"]
        .as_u64()
        .is_none_or(|n| n > 16 * 1024 * 1024)
    {
        return Err(Fail::new("JUnit больше 16 МиБ или размер неизвестен"));
    }
    let run = phase
        .run_id
        .as_ref()
        .ok_or_else(|| Fail::new("нет run_id"))?;
    let id = artifact["id"]
        .as_str()
        .ok_or_else(|| Fail::new("нет JUnit artifact id"))?;
    let destination = root.join(format!("cases-{index}-{}", ferry_core::jobs::unique_id()));
    let mut listing = json!({"run_id":run,"artifacts":[artifact]});
    crate::agent_tools::download(device, &mut listing, id, destination.to_str())?;
    let raw = listing["downloaded"][0]
        .as_str()
        .ok_or_else(|| Fail::new("нет полученного JUnit"))?;
    let data = ferry_core::junit::read_cases(Path::new(raw))?;
    let file = destination.join("cases.json");
    journal::write(&file, &data)?;
    Ok(
        json!({"file":file,"raw_file":raw,"count":data.cases.len(),"complete":data.complete,"omitted_cases":data.omitted_cases,
        "note":"Индекс ограничен 20 000 testcase / 4 МиБ; полный исходный JUnit сохранён в raw_file. Сообщения/стек в индексе могут быть обрезаны."}),
    )
}

fn hydrate(root: &Path, op: &Operation) -> Result<(), Fail> {
    let guard = journal::lock(root)?;
    guard
        .try_lock()
        .map_err(|_| Fail::new("проверка ещё выполняется"))?;
    for (index, (device, _)) in op.targets.iter().enumerate() {
        let mut target = journal::target(root, op, index)?;
        if !journal::terminal(&target.state) || target.cleanup_pending {
            return Err(Fail::new("проверка ещё не завершена"));
        }
        if target.row["test_cases"]["file"].is_string() {
            continue;
        }
        let Some(junit) = op.profile.junit.as_ref() else {
            continue;
        };
        let Some(phase) = target.phases.iter().find(|p| p.name == "test").cloned() else {
            continue;
        };
        let artifact = target.row["collected"]
            .get(junit)
            .cloned()
            .unwrap_or(Value::Null);
        target.row["test_cases"] = capture_cases(root, index, device, &phase, &artifact)
            .unwrap_or_else(|e| json!({"complete":false,"error":e.to_string()}));
        journal::write(&journal::target_path(root, index), &target)?;
    }
    Ok(())
}

fn cases(root: &Path, report: &Value) -> Result<BTreeMap<String, Value>, Fail> {
    let mut found = BTreeMap::new();
    let rows = report["results"]
        .as_array()
        .ok_or_else(|| Fail::new("нет результатов"))?;
    if rows.is_empty()
        || report["worker_active"] == true
        || !matches!(report["state"].as_str(), Some("succeeded" | "failed"))
    {
        return Err(Fail::new(
            "сравнивать можно только завершённые succeeded/failed проверки",
        ));
    }
    for row in rows {
        if row["test_cases"]["complete"] != true
            || row["cleanup_pending"] == true
            || row["tests"]["warnings"]
                .as_array()
                .is_none_or(|w| !w.is_empty())
        {
            return Err(Fail::new(format!(
                "{}: нет полного подтверждённого набора JUnit testcase",
                row["device"]
            )));
        }
        let file = row["test_cases"]["file"]
            .as_str()
            .ok_or_else(|| Fail::new("нет testcase index"))?;
        let path = std::path::PathBuf::from(file).canonicalize()?;
        if !path.starts_with(root.canonicalize()?) {
            return Err(Fail::new("testcase index вне журнала проверки"));
        }
        let data: Value = journal::read(&path)?;
        if data["complete"] != true {
            return Err(Fail::new("неполный testcase index"));
        }
        for case in data["cases"]
            .as_array()
            .ok_or_else(|| Fail::new("нет cases"))?
        {
            if case["identity_complete"] != true {
                return Err(Fail::new("неполная идентичность testcase"));
            }
            let key = serde_json::to_string(&json!([
                row["device"],
                case["suite"],
                case["class"],
                case["name"],
                case["file"]
            ]))
            .map_err(Fail::new)?;
            if found.insert(key, case.clone()).is_some() {
                return Err(Fail::new(
                    "повторяющиеся testcase identity: сравнение неоднозначно",
                ));
            }
        }
    }
    Ok(found)
}

fn diff(before: BTreeMap<String, Value>, mut after: BTreeMap<String, Value>) -> Value {
    let (mut regressions, mut improvements, mut removed, mut added, mut changed) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut unchanged = 0;
    for (id, old) in before {
        let Some(new) = after.remove(&id) else {
            removed.push(json!({"identity":id,"before":old}));
            continue;
        };
        let row = json!({"identity":id,"before":old,"after":new});
        let bad = |v: &Value| matches!(v["state"].as_str(), Some("failed" | "error"));
        if (old["state"] == "passed" && new["state"] != "passed")
            || (old["state"] == "skipped" && bad(&new))
        {
            regressions.push(row);
        } else if bad(&old) && new["state"] == "passed" {
            improvements.push(row);
        } else if old["state"] != new["state"] {
            changed.push(row);
        } else {
            unchanged += 1;
        }
    }
    for (id, new) in after {
        added.push(json!({"identity":id,"after":new}));
    }
    json!({"regressions":regressions,"improvements":improvements,"added":added,"removed":removed,"changed":changed,"unchanged":unchanged})
}

pub(super) fn compare(baseline: &str, candidate: &str) -> io::Result<i32> {
    finish((|| {
        let a = journal::root(baseline)?;
        let b = journal::root(candidate)?;
        let oa: Operation = journal::read(&a.join("operation.json"))?;
        let ob: Operation = journal::read(&b.join("operation.json"))?;
        let hydration = (|| {
            hydrate(&a, &oa)?;
            hydrate(&b, &ob)
        })()
        .err();
        let ra = journal::report(&a)?;
        let rb = journal::report(&b)?;
        let outcome = (|| {
            if let Some(error) = hydration {
                return Err(error);
            }
            if oa.profile != ob.profile || oa.profile_name != ob.profile_name {
                return Err(Fail::new(
                    "разные профили: команды/условия/артефакты несопоставимы",
                ));
            }
            let project = |op: &Operation| {
                op.project
                    .as_ref()
                    .map(|p| (p.name.clone(), p.prefix.clone()))
            };
            if project(&oa) != project(&ob) || (oa.project.is_none() && oa.cwd != ob.cwd) {
                return Err(Fail::new(
                    "разные проекты: совпадение testcase имён не доказывает сопоставимость",
                ));
            }
            let devices = |op: &Operation| {
                op.targets
                    .iter()
                    .map(|(d, s)| (d.clone(), s.as_ref().map(|s| s.os.clone())))
                    .collect::<Vec<_>>()
            };
            if devices(&oa) != devices(&ob) {
                return Err(Fail::new("разные устройства/ОС/порядок матрицы"));
            }
            Ok(diff(cases(&a, &ra)?, cases(&b, &rb)?))
        })();
        let mut value = match outcome {
            Ok(mut v) => {
                v["comparable"] = true.into();
                v["errors"] = json!([]);
                v
            }
            Err(e) => {
                json!({"comparable":false,"errors":[e.to_string()],"regressions":[],"improvements":[],"added":[],"removed":[],"changed":[],"unchanged":0})
            }
        };
        value["schema_version"] = 1.into();
        value["baseline"] = json!({"id":baseline,"project":ra["project"]});
        value["candidate"] = json!({"id":candidate,"project":rb["project"]});
        value["note"]="Сравнение точных identity [device,suite,class,name,file]; удалённые/добавленные тесты не считаются исправлениями. Тайминги не являются performance benchmark.".into();
        let failed = value["comparable"] != true
            || value["regressions"]
                .as_array()
                .is_some_and(|v| !v.is_empty())
            || value["removed"].as_array().is_some_and(|v| !v.is_empty())
            || value["added"].as_array().is_some_and(|v| {
                v.iter()
                    .any(|r| matches!(r["after"]["state"].as_str(), Some("failed" | "error")))
            });
        println!("{value}");
        Ok(i32::from(failed))
    })())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn comparison_does_not_turn_skips_or_removed_tests_into_success() {
        let before = BTreeMap::from([
            ("same".into(), json!({"state":"passed"})),
            ("gone".into(), json!({"state":"passed"})),
            ("fixed".into(), json!({"state":"error"})),
            ("skip".into(), json!({"state":"skipped"})),
        ]);
        let after = BTreeMap::from([
            ("same".into(), json!({"state":"skipped"})),
            ("new".into(), json!({"state":"passed"})),
            ("fixed".into(), json!({"state":"passed"})),
            ("skip".into(), json!({"state":"failed"})),
        ]);
        let v = diff(before, after);
        assert_eq!(v["regressions"].as_array().unwrap().len(), 2);
        for key in ["improvements", "removed", "added"] {
            assert_eq!(v[key].as_array().unwrap().len(), 1);
        }
    }
    #[test]
    fn pagination_is_bounded_and_cursor_is_validated() {
        let root =
            std::env::temp_dir().join(format!("ferry-history-{}", ferry_core::jobs::unique_id()));
        fs::create_dir_all(&root).unwrap();
        assert_eq!(listing(&root, 1, None).unwrap()["checks"], json!([]));
        assert!(listing(&root, 1, Some("1:../bad")).is_err());
        for (id, created) in [("a", 1), ("b", 2), ("c", 2)] {
            let dir = root.join(id);
            fs::create_dir(&dir).unwrap();
            let op = Operation {
                id: id.into(),
                profile_name: "smoke".into(),
                profile: serde_json::from_value(json!({"platforms":{"all":{"command":"true"}}}))
                    .unwrap(),
                cwd: root.clone(),
                project: None,
                isolated: false,
                targets: vec![(None, None)],
                parallel: 1,
                created,
            };
            journal::write(&dir.join("operation.json"), &op).unwrap();
        }
        let first = listing(&root, 1, None).unwrap();
        assert_eq!(first["checks"][0]["id"], "c");
        assert_eq!(first["next_cursor"], "2:c");
        let second = listing(&root, 1, first["next_cursor"].as_str()).unwrap();
        assert_eq!(second["checks"][0]["id"], "b");
        assert_eq!(second["next_cursor"], "2:b");
        let third = listing(&root, 1, second["next_cursor"].as_str()).unwrap();
        assert_eq!(third["checks"][0]["id"], "a");
        assert!(third["next_cursor"].is_null());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn duplicate_or_incomplete_test_identity_is_incomparable() {
        let root =
            std::env::temp_dir().join(format!("ferry-compare-{}", ferry_core::jobs::unique_id()));
        fs::create_dir_all(&root).unwrap();
        let file = root.join("cases.json");
        let case = json!({"suite":["suite"],"class":"class","name":"same","file":"","state":"passed","identity_complete":true});
        journal::write(&file, &json!({"complete":true,"cases":[case,case]})).unwrap();
        let report = json!({"state":"succeeded","worker_active":false,"results":[{"device":"local","test_cases":{"file":file,"complete":true},"tests":{"warnings":[]},"cleanup_pending":false}]});
        assert!(
            cases(&root, &report)
                .unwrap_err()
                .to_string()
                .contains("повторяющиеся")
        );
        journal::write(&file, &json!({"complete":false,"cases":[case]})).unwrap();
        assert!(
            cases(&root, &report)
                .unwrap_err()
                .to_string()
                .contains("неполный")
        );
        fs::remove_dir_all(root).unwrap();
    }
}
