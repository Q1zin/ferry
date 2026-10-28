//! Explicit, bounded copies make a diagnostic report usable after the peer disconnects.
use super::*;
use std::path::Path;

const FILE_BYTES: u64 = 64 * 1024 * 1024;
const TOTAL_BYTES: u64 = 128 * 1024 * 1024;
const MAX_FILES: usize = 32;

fn selection(listing: &Value, selected: &[String]) -> Result<Vec<Value>, Fail> {
    let files = listing["artifacts"]
        .as_array()
        .ok_or_else(|| Fail::new("нет списка артефактов"))?;
    if selected.is_empty() {
        return Ok(files.clone());
    }
    let mut result = Vec::new();
    for id in selected {
        if result.iter().any(|f: &Value| f["id"] == *id) {
            continue;
        }
        result.push(
            files
                .iter()
                .find(|f| f["id"] == *id)
                .ok_or_else(|| Fail::new(format!("нет артефакта {id}")))?
                .clone(),
        );
    }
    Ok(result)
}

pub(crate) fn collect(
    device: &Option<String>,
    value: &mut Value,
    report: &Path,
    selected: &[String],
) -> Result<(), Fail> {
    collect_budget(device, value, report, selected, TOTAL_BYTES)
}

pub(crate) fn collect_budget(
    device: &Option<String>,
    value: &mut Value,
    report: &Path,
    selected: &[String],
    budget: u64,
) -> Result<(), Fail> {
    let budget = budget.min(TOTAL_BYTES);
    let dir = report
        .parent()
        .ok_or_else(|| Fail::new("нет каталога отчёта"))?;
    let destination = dir.join("files");
    let mut copied = Vec::new();
    let mut errors = Vec::new();
    let mut total = 0u64;
    let files = match selection(&value["evidence"], selected) {
        Ok(files) => files,
        Err(e) => {
            errors.push(e.to_string());
            Vec::new()
        }
    };
    for item in files {
        let id = item["id"].as_str().unwrap_or_default();
        let bytes = item["bytes"].as_u64();
        let outcome = (|| -> Result<Value, Fail> {
            let bytes = bytes.ok_or_else(|| Fail::new("размер неизвестен"))?;
            if copied.len() >= MAX_FILES
                || bytes > FILE_BYTES
                || bytes > budget.saturating_sub(total)
            {
                return Err(Fail::new(
                    "предел пакета: 32 файла, 64 МиБ на файл, 128 МиБ всего",
                ));
            }
            if id == "log" && value["result"]["state"] == "running" {
                return Err(Fail::new(
                    "лог ещё записывается; повтори после завершения запуска",
                ));
            }
            let mut listing = value["evidence"].clone();
            crate::agent_tools::download(device, &mut listing, id, destination.to_str())?;
            let paths = listing["downloaded"]
                .as_array()
                .ok_or_else(|| Fail::new("нет полученных файлов"))?;
            if paths.len() != 1 {
                return Err(Fail::new("артефакт должен содержать один файл"));
            }
            let path = Path::new(
                paths[0]
                    .as_str()
                    .ok_or_else(|| Fail::new("нет пути файла"))?,
            );
            let metadata = std::fs::symlink_metadata(path)?;
            if !metadata.is_file() || metadata.len() != bytes {
                return Err(Fail::new("тип или размер файла изменился при получении"));
            }
            let relative = path
                .strip_prefix(dir)
                .map_err(|_| Fail::new("файл вне пакета"))?;
            total += bytes;
            Ok(json!({"id":id,"name":item["name"],"bytes":bytes,
                "file":relative.to_string_lossy().replace('\\', "/")}))
        })();
        match outcome {
            Ok(file) => copied.push(file),
            Err(e) => errors.push(format!("{id}: {e}")),
        }
    }
    if dir.join("window.png").is_file() {
        value["observation"]["screenshot"] = "window.png".into();
    }
    value["bundle"] = json!({"complete":errors.is_empty(),"files":copied,"bytes":total,
        "errors":errors,"path_base":"report_directory","limits":{"files":MAX_FILES,"file_bytes":FILE_BYTES,"total_bytes":budget}});
    ferry_core::write_private(
        report,
        &serde_json::to_string_pretty(value).map_err(|e| Fail::new(e.to_string()))?,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_selection_deduplicates_and_rejects_unknown_files() {
        let listing = json!({"artifacts":[{"id":"log"},{"id":"one"}]});
        assert_eq!(selection(&listing, &[]).unwrap().len(), 2);
        assert_eq!(
            selection(&listing, &["one".into(), "one".into()]).unwrap(),
            vec![json!({"id":"one"})]
        );
        assert!(selection(&listing, &["missing".into()]).is_err());
    }

    #[test]
    fn portable_bundle_contains_relative_copies_and_reports_oversize() {
        let root =
            std::env::temp_dir().join(format!("ferry-bundle-{}", ferry_core::jobs::unique_id()));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("source.txt");
        std::fs::write(&source, "proof").unwrap();
        let dir = root.join("bundle");
        std::fs::create_dir(&dir).unwrap();
        let mut value = json!({"result":{"state":"succeeded"},"evidence":{"run_id":"run-one","artifacts":[
            {"id":"proof","name":"source.txt","path":source,"bytes":5},
            {"id":"large","bytes":FILE_BYTES+1}
        ]}});
        collect(&None, &mut value, &dir.join("report.json"), &[]).unwrap();
        assert_eq!(value["bundle"]["complete"], false);
        assert_eq!(value["bundle"]["bytes"], 5);
        let file = value["bundle"]["files"][0]["file"].as_str().unwrap();
        assert!(!Path::new(file).is_absolute());
        assert_eq!(std::fs::read_to_string(dir.join(file)).unwrap(), "proof");
        assert_eq!(value["bundle"]["errors"].as_array().unwrap().len(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }
}
