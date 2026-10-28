use super::*;
use journal::Operation;
use std::{
    fs, io,
    path::{Path, PathBuf},
};
const TOTAL: u64 = 512 * 1024 * 1024;

fn private_dir(path: &Path) -> Result<(), Fail> {
    fs::create_dir(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
fn copy_local(source: &str, dest: &Path, limit: u64) -> Result<u64, Fail> {
    let source = Path::new(source);
    let meta = fs::symlink_metadata(source)?;
    if !meta.is_file() || meta.is_symlink() || meta.len() > limit {
        return Err(Fail::new(
            "небезопасный или слишком большой локальный артефакт",
        ));
    }
    let source = source.canonicalize()?;
    if !source.starts_with(ferry_core::ferry_home()?.canonicalize()?) {
        return Err(Fail::new("артефакт вне ~/.ferry"));
    }
    fs::copy(source, dest)?;
    Ok(meta.len())
}
fn zip(dir: &Path, path: &Path) -> Result<(), Fail> {
    fn visit(
        root: &Path,
        dir: &Path,
        zip: &mut zip::ZipWriter<fs::File>,
        bytes: &mut u64,
    ) -> Result<(), Fail> {
        let mut files = fs::read_dir(dir)?
            .map(|e| e.map(|e| e.path()))
            .collect::<Result<Vec<_>, _>>()?;
        files.sort();
        for path in files {
            let meta = fs::symlink_metadata(&path)?;
            if meta.is_symlink() {
                return Err(Fail::new("ссылки не включаются в bug report"));
            }
            if meta.is_dir() {
                visit(root, &path, zip, bytes)?;
                continue;
            }
            if !meta.is_file() {
                return Err(Fail::new("необычный файл в bug report"));
            }
            *bytes = bytes.saturating_add(meta.len());
            if *bytes > TOTAL + 64 * 1024 * 1024 {
                return Err(Fail::new("bug report больше 576 МиБ"));
            }
            let name = path
                .strip_prefix(root)
                .map_err(Fail::new)?
                .to_string_lossy()
                .replace('\\', "/");
            zip.start_file(
                name,
                zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored),
            )
            .map_err(Fail::new)?;
            io::copy(&mut fs::File::open(path)?, zip)?;
        }
        Ok(())
    }
    let mut options = fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    let mut zip = zip::ZipWriter::new(file);
    visit(dir, dir, &mut zip, &mut 0)?;
    zip.finish().map_err(Fail::new)?;
    Ok(())
}

fn build(root: &Path, op: &Operation, directory: &Path) -> Result<Value, Fail> {
    let mut report = journal::report(root)?;
    let mut errors = Vec::new();
    let mut phases = Vec::new();
    let mut copied = 0u64;
    if report["worker_active"] == true {
        errors.push("проверка ещё выполняется: пакет является промежуточным".to_string());
    }
    if report["state"] == "interrupted" {
        errors
            .push("проверка прервана: сначала resume/collect либо сохрани частичный пакет".into());
    }
    journal::write(&directory.join("operation.json"), op)?;
    for (index, (device, _)) in op.targets.iter().enumerate() {
        let target = journal::target(root, op, index)?;
        let dir = directory.join(format!("target-{index}"));
        private_dir(&dir)?;
        if target.phases.is_empty() {
            errors.push(format!("target {index}: фазы не запускались"));
        }
        for (n, phase) in target.phases.iter().enumerate() {
            let Some(run) = &phase.run_id else {
                errors.push(format!(
                    "target {index}/{}: нет подтверждённого run_id",
                    phase.name
                ));
                continue;
            };
            let phase_dir = dir.join(format!("phase-{n}"));
            private_dir(&phase_dir)?;
            let file = phase_dir.join("report.json");
            let junit = if phase.name == "test" {
                op.profile
                    .junit
                    .as_ref()
                    .and_then(|p| target.row["collected"].get(p))
                    .and_then(|a| a["id"].as_str())
            } else {
                None
            };
            let mut value = diagnostics::collect(device, &phase.job, run, None, junit, None);
            for key in [
                "result",
                "evidence",
                "run_metadata",
                "environment",
                "tool_versions",
            ] {
                if let Some(error) = value[key]["error"].as_str() {
                    errors.push(format!("target {index}/{}: {key}: {error}", phase.name));
                }
            }
            diagnostics::bundle::collect_budget(
                device,
                &mut value,
                &file,
                &[],
                TOTAL.saturating_sub(copied),
            )?;
            copied = copied.saturating_add(value["bundle"]["bytes"].as_u64().unwrap_or(0));
            if value["bundle"]["complete"] != true {
                errors.push(format!(
                    "target {index}/{}: неполный per-run bundle",
                    phase.name
                ));
            }
            // Parse every bounded testcase from the downloaded original XML, not from the failure summary.
            if let Some(id) = junit {
                let relative = value["bundle"]["files"]
                    .as_array()
                    .and_then(|v| v.iter().find(|v| v["id"] == id))
                    .and_then(|v| v["file"].as_str());
                match relative
                    .map(|p| ferry_core::junit::read_cases(&phase_dir.join(p)))
                    .transpose()
                {
                    Ok(Some(cases)) => {
                        journal::write(&phase_dir.join("cases.json"), &cases)?;
                        value["test_cases"] = json!({"file":"cases.json","complete":cases.complete,"count":cases.cases.len(),"omitted_cases":cases.omitted_cases,"original_xml":relative});
                    }
                    Ok(None) => errors.push(format!("target {index}: исходный JUnit не получен")),
                    Err(e) => errors.push(format!("target {index}: JUnit: {e}")),
                }
            }
            journal::write(&file, &value)?;
            phases.push(json!({"target":index,"device":device.as_deref().unwrap_or("local"),"phase":phase.name,"job":phase.job,"run_id":run,"report":format!("target-{index}/phase-{n}/report.json"),"complete":value["bundle"]["complete"]}));
        }
        for (key, field, filename) in [
            ("final_observation", "screenshot", "window.png"),
            ("recording", "video_file", "video.mp4"),
            ("recording", "manifest_file", "video-manifest.json"),
        ] {
            if let Some(source) = target.row[key][field].as_str() {
                match copy_local(source, &dir.join(filename), TOTAL.saturating_sub(copied)) {
                    Ok(bytes) => {
                        copied += bytes;
                        report["results"][index][key][field] =
                            format!("target-{index}/{filename}").into();
                        if field == "manifest_file" {
                            let file = dir.join(filename);
                            let mut manifest: Value = journal::read(&file)?;
                            manifest["recording"]["video_file"] = "video.mp4".into();
                            manifest["recording"]["manifest_file"] = filename.into();
                            manifest["recording"]
                                .as_object_mut()
                                .map(|r| r.remove("frame_spool"));
                            if let Some(frames) = manifest["frames"].as_array_mut() {
                                for frame in frames {
                                    if let Some(frame) = frame.as_object_mut() {
                                        frame.remove("file");
                                    }
                                }
                            }
                            manifest["frames_included"] = false.into();
                            journal::write(&file, &manifest)?;
                        }
                    }
                    Err(e) => errors.push(format!("target {index}/{key}: {e}")),
                }
            }
        }
        if let Some(row) = report["results"][index].as_object_mut() {
            row.remove("diagnostic_file");
            row.remove("diagnostics");
            row.remove("test_cases");
        }
        if let Some(recording) = report["results"][index]["recording"].as_object_mut() {
            recording.remove("frame_spool");
        }
    }
    report.as_object_mut().unwrap().remove("journal");
    report.as_object_mut().unwrap().remove("report_file");
    report["bundle"] = json!({"schema_version":1,"complete":errors.is_empty(),"phases":phases,"bytes":copied,"errors":errors,"path_base":"report_directory","note":"Отчёт содержит команды, пути, логи и явно собранные артефакты. Переменные окружения автоматически не включаются. Перед передачей проверь содержимое."});
    journal::write(&directory.join("report.json"), &report)?;
    fs::write(
        directory.join("README.txt"),
        "Ferry check report\nOpen report.json. Each target/phase references its per-run report.json and relative files. Original JUnit XML retains all testcase details; cases.json is a bounded index. A partial bundle lists every missing item in errors. No issue/ticket was created.\n",
    )?;
    Ok(report)
}

pub(super) fn run(id: &str, args: &[&str]) -> io::Result<i32> {
    let requested = match args {
        [] => None,
        ["--dest", dir] => Some(*dir),
        _ => return crate::usage(),
    };
    finish((|| {
        let root = journal::root(id)?;
        let op: Operation = journal::read(&root.join("operation.json"))?;
        let directory = match requested {
            Some(p) => std::path::absolute(p)?,
            None => ferry_core::ferry_home()?
                .join("diagnostics")
                .join(format!("check-{id}-{}", ferry_core::jobs::unique_id())),
        };
        if let Some(parent) = directory.parent() {
            fs::create_dir_all(parent)?;
        }
        private_dir(&directory)?;
        let report = build(&root, &op, &directory)?;
        let archive = PathBuf::from(format!("{}.zip", directory.to_string_lossy()));
        zip(&directory, &archive)?;
        let value = json!({"schema_version":1,"id":id,"directory":directory,"report_file":directory.join("report.json"),"archive_file":archive,"complete":report["bundle"]["complete"],"errors":report["bundle"]["errors"]});
        println!("{value}");
        Ok(i32::from(value["complete"] != true))
    })())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    #[test]
    fn archive_is_self_contained_and_refuses_overwrite() {
        let root = std::env::temp_dir().join(format!(
            "ferry-check-export-{}",
            ferry_core::jobs::unique_id()
        ));
        private_dir(&root).unwrap();
        fs::write(root.join("report.json"), "{\"complete\":true}").unwrap();
        let archive = root.with_extension("zip");
        zip(&root, &archive).unwrap();
        assert!(zip(&root, &archive).is_err());
        let mut zip = zip::ZipArchive::new(fs::File::open(&archive).unwrap()).unwrap();
        let mut text = String::new();
        zip.by_name("report.json")
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "{\"complete\":true}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&archive).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        fs::remove_dir_all(root).unwrap();
        fs::remove_file(archive).unwrap();
    }
}
