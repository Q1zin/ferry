use super::*;

pub(super) fn run(id: &str) -> io::Result<i32> {
    finish((|| {
        let root = journal::root(id)?;
        let report = if !root.join("operation.json").try_exists()?
            && history::legacy_snapshot(&root)?
        {
            let file = root.join("report.json");
            let saved: Value = journal::read(&file)?;
            json!({"id":id,"profile":saved["profile"],"state":saved["state"],"report_file":file,
                "legacy":true,"results":saved["results"].as_array().cloned().unwrap_or_else(|| vec![saved])})
        } else {
            journal::report(&root)?
        };
        println!("{}", summarize(&report));
        Ok(0)
    })())
}

fn brief(value: &Value, limit: usize) -> Option<String> {
    value
        .as_str()
        .filter(|s| !s.is_empty())
        .map(|s| s.chars().take(limit).collect())
}

fn summarize(report: &Value) -> Value {
    let id = &report["id"];
    let rows = report["results"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let targets: Vec<_> = rows.iter().map(target).collect();
    let next = if report["legacy"] == true {
        Value::Null
    } else if report["worker_active"] == true {
        json!(["ferry", "check", "status", id])
    } else if report["state"] == "interrupted" {
        let action = if rows
            .iter()
            .any(|r| r["collection_error"].is_object() || r["cleanup_pending"] == true)
        {
            "collect"
        } else {
            "status"
        };
        json!(["ferry", "check", action, id])
    } else {
        targets
            .iter()
            .filter_map(|r| r["phases"].as_array())
            .flatten()
            .find(|p| p["state"] == "failed" && p["log_argv"].is_array())
            .map_or_else(
                || json!(["ferry", "check", "export", id]),
                |p| p["log_argv"].clone(),
            )
    };
    json!({"schema_version":1,"id":id,"profile":report["profile"],"state":report["state"],
        "report_file":report["report_file"],"legacy":report["legacy"] == true,
        "targets":targets,"next_argv":next})
}

fn target(row: &Value) -> Value {
    let device = row["device_id"]
        .as_str()
        .or_else(|| row["device"].as_str())
        .unwrap_or("local");
    let phases: Vec<_> = row["phases"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|phase| {
            let result = &phase["result"];
            let log_argv = phase["job"]
                .as_str()
                .zip(phase["run_id"].as_str())
                .map(|(job, run)| {
                    let mut args = vec!["ferry", "result"];
                    if device != "local" {
                        args.push(device);
                    }
                    args.extend([job, "--run", run]);
                    args
                });
            let failed = matches!(result["state"].as_str(), Some("failed" | "lost" | "killed"));
            json!({"name":phase["name"],"job":phase["job"],"run_id":phase["run_id"],
            "state":result["state"],"exit_code":result["exit_code"],"log_argv":log_argv,
            "log_excerpt":failed.then(|| brief(&phase["carry"],600)
                .or_else(|| brief(&result["log"],600))).flatten()})
        })
        .collect();
    let artifacts: Vec<_> = row["artifact_collection"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(_, a)| a["state"] != "collected")
        .map(|(path, a)| json!({"path":path,"state":a["state"],"error":brief(&a["error"],240)}))
        .take(8)
        .collect();
    let tests = &row["tests"];
    let failures: Vec<_> = tests["failures"]
        .as_array()
        .into_iter()
        .flatten()
        .take(3)
        .map(|test| {
            json!({"name":brief(&test["name"],180),"class":brief(&test["class"],180),
            "file":brief(&test["file"],240),"line":test["line"],"state":test["state"],
            "message":brief(&test["message"],360)})
        })
        .collect();
    let junit = &row["junit_analysis"];
    let recording = &row["recording"];
    let last = &row["result"];
    let collection = &row["collection_error"];
    let errors: Vec<_> = row["errors"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| brief(e, 280))
        .take(4)
        .collect();
    json!({
        "device":row["device"],"state":row["state"],"phases":phases,"timed_out":row["timed_out"] == true,
        "last_result":(phases.is_empty() && last.is_object()).then(|| json!({
            "state":last["state"],"exit_code":last["exit_code"],"log_excerpt":brief(&last["log"],600)
        })),
        "cleanup_pending":row["cleanup_pending"] == true,"errors":errors,
        "artifact_issues":artifacts,
        "artifact_issue_count":row["artifact_collection"].as_object().into_iter().flat_map(|m| m.values())
            .filter(|a| a["state"] != "collected").count(),
        "collection_error":collection.is_object().then(|| json!({
            "phase":collection["phase"],"state":collection["state"],"error":brief(&collection["error"],300)
        })),
        "junit":(junit.is_object() || tests.is_object()).then(|| json!({
            "state":junit["state"],"path":junit["path"],"artifact_id":junit["artifact_id"],
            "error":brief(&junit["error"],300),"tests":tests["tests"],"failed":tests["failed"],
            "errors":tests["errors"],"failures":failures,"raw_file":row["test_cases"]["raw_file"],
            "index_error":brief(&row["test_cases"]["error"],300),
        })),
        "recording":recording.is_object().then(|| json!({
            "state":recording["state"],"complete":recording["complete"],"video_file":recording["video_file"],
            "manifest_file":recording["manifest_file"],"error":brief(&recording["error"],300),
            "capture_error":brief(&recording["capture_error"],300),
            "encoding_error":brief(&recording["encoding_error"],300),
        })),
        "diagnostic_file":row["diagnostic_file"],"screenshot":row["final_observation"]["screenshot"],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explains_exit_and_collected_evidence_independently() {
        let value = summarize(&json!({"id":"example","state":"failed","results":[{
            "device":"win","state":"failed",
            "phases":[{"name":"test","job":"check-test","run_id":"exact","result":{"state":"succeeded","exit_code":0}}],
            "artifact_collection":{"junit.xml":{"state":"missing_on_device"},"extra.log":{"state":"collection_error"}},
            "recording":{"state":"failed","encoding_error":"encoder exited"}
        }]}));
        let row = &value["targets"][0];
        assert_eq!(row["phases"][0]["exit_code"], 0);
        assert_eq!(
            row["phases"][0]["log_argv"],
            json!(["ferry", "result", "win", "check-test", "--run", "exact"])
        );
        assert!(
            row["artifact_issues"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["path"] == "junit.xml" && a["state"] == "missing_on_device")
        );
        assert!(row["junit"].is_null());
        assert_eq!(row["recording"]["encoding_error"], "encoder exited");
    }
}
