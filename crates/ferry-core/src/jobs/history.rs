//! Очистка только архивов одной задачи под общей блокировкой start/evidence.
use super::*;
use serde_json::{Value, json};
use std::hash::{DefaultHasher, Hash, Hasher};

fn size(path: &Path, hash: &mut DefaultHasher, depth: usize) -> io::Result<u64> {
    if depth > 8 {
        return Err(io::Error::other("слишком глубокая история"));
    }
    let meta = fs::symlink_metadata(path)?;
    if meta.is_symlink() {
        return Err(io::Error::other("ссылка в истории: очистка отменена"));
    }
    path.hash(hash);
    meta.len().hash(hash);
    meta.modified()?.hash(hash);
    if meta.is_file() {
        return Ok(meta.len());
    }
    let mut entries = fs::read_dir(path)?
        .map(|e| e.map(|e| e.path()))
        .collect::<io::Result<Vec<_>>>()?;
    entries.sort();
    let mut total = 0u64;
    for entry in entries {
        total = total.saturating_add(size(&entry, hash, depth + 1)?);
    }
    Ok(total)
}
pub(super) fn storage(
    root: &Path,
    name: &str,
    days: Option<u32>,
    budget: Option<u64>,
    apply: Option<&str>,
) -> io::Result<Value> {
    if apply.is_some() && days.is_none() && budget.is_none() {
        return Err(io::Error::other("нужна политика очистки"));
    }
    let mut hash = DefaultHasher::new();
    name.hash(&mut hash);
    days.hash(&mut hash);
    budget.hash(&mut hash);
    let current = root.join("jobs").join(name);
    let current_bytes = if current.exists() {
        size(&current, &mut DefaultHasher::new(), 0)?
    } else {
        0
    };
    let folder = root.join("runs").join(name);
    let mut rows = Vec::new();
    if folder.exists() {
        if fs::symlink_metadata(&folder)?.is_symlink() {
            return Err(io::Error::other("ссылка вместо истории"));
        }
        for entry in fs::read_dir(&folder)? {
            if rows.len() >= 5000 {
                return Err(io::Error::other(
                    "больше 5000 архивов одной задачи; очистка отменена",
                ));
            }
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                return Err(io::Error::other("неожиданный файл в истории"));
            }
            let id = entry.file_name().to_string_lossy().into_owned();
            validate_name(&id)?;
            let dir = entry.path();
            let mut fingerprint = DefaultHasher::new();
            let bytes = size(&dir, &mut fingerprint, 0)?;
            let started = read(&dir, "started").and_then(|s| s.parse::<u64>().ok());
            rows.push((dir.clone(), json!({"run_id":id,"bytes":bytes,"started":started,"pinned":dir.join("pinned").exists(),"fingerprint":format!("{:016x}",fingerprint.finish())})));
        }
    }
    rows.sort_by_key(|(_, r)| {
        (
            r["started"].as_u64().unwrap_or(u64::MAX),
            r["run_id"].as_str().unwrap_or_default().to_string(),
        )
    });
    let total: u64 = rows.iter().map(|(_, r)| r["bytes"].as_u64().unwrap()).sum();
    let mut remaining = total;
    let mut candidates = Vec::new();
    for (dir, row) in &rows {
        row.to_string().hash(&mut hash);
        let old = days.is_some_and(|d| {
            row["started"]
                .as_u64()
                .is_some_and(|s| now().saturating_sub(s) >= u64::from(d) * 86400)
        });
        if row["pinned"] == false && (old || budget.is_some_and(|b| remaining > b)) {
            remaining = remaining.saturating_sub(row["bytes"].as_u64().unwrap());
            candidates.push((dir.clone(), row.clone()));
        }
    }
    let ids: Vec<_> = candidates
        .iter()
        .map(|(_, r)| r["run_id"].clone())
        .collect();
    serde_json::to_string(&ids)?.hash(&mut hash);
    let token = format!("{:016x}", hash.finish());
    if apply.is_some_and(|t| t != token) {
        return Err(io::Error::other(
            "история изменилась или неверный token; сначала повтори preview",
        ));
    }
    let mut deleted = Vec::new();
    let mut errors = Vec::new();
    if apply.is_some() {
        for (dir, row) in &candidates {
            match fs::remove_dir_all(dir) {
                Ok(()) => deleted.push(row["run_id"].clone()),
                Err(e) => errors.push(json!({"run_id":row["run_id"],"error":e.to_string()})),
            }
        }
    }
    Ok(
        json!({"name":name,"current_bytes":current_bytes,"archive_bytes":total,"planned_remaining_archive_bytes":remaining,"budget_met":budget.is_none_or(|b|remaining<=b),"runs":rows.into_iter().map(|(_,r)|r).collect::<Vec<_>>(),"candidates":candidates.into_iter().map(|(_,r)|r).collect::<Vec<_>>(),"preview_token":token,"applied":apply.is_some(),"deleted":deleted,"errors":errors}),
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preview_pin_and_stale_plan_protect_data() {
        let root = std::env::temp_dir().join(unique_id());
        for id in ["a", "b"] {
            let dir = root.join("runs/test").join(id);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("log"), "proof").unwrap();
            fs::write(dir.join("started"), "1").unwrap();
        }
        fs::write(root.join("runs/test/a/pinned"), "yes").unwrap();
        let preview = storage(&root, "test", None, Some(0), None).unwrap();
        assert_eq!(preview["candidates"].as_array().unwrap().len(), 1);
        assert_eq!(preview["budget_met"], false);
        fs::write(root.join("runs/test/b/log"), "new proof").unwrap();
        assert!(
            storage(
                &root,
                "test",
                None,
                Some(0),
                preview["preview_token"].as_str()
            )
            .is_err()
        );
        let preview = storage(&root, "test", None, Some(0), None).unwrap();
        let done = storage(
            &root,
            "test",
            None,
            Some(0),
            preview["preview_token"].as_str(),
        )
        .unwrap();
        assert_eq!(done["deleted"], json!(["b"]));
        assert!(root.join("runs/test/a/log").exists());
        fs::remove_dir_all(root).unwrap();
    }
}
