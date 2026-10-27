//! Isolated project copies and a preview before returning their changes.
use crate::jobs::execution::Source;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs, io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Action {
    Status,
    Seal,
    Prepare,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub source: Source,
    pub action: Action,
}
fn component(s: &str) -> io::Result<()> {
    if s.is_empty()
        || s.len() > 120
        || s.starts_with('.')
        || s.starts_with('-')
        || !s
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-".contains(c))
    {
        return Err(io::Error::other(
            "workspace: имя должно состоять из букв ASCII, цифр, _ и - (до 120)",
        ));
    }
    Ok(())
}
pub fn source(project: &str, id: &str, commit: &str, tree: &str) -> io::Result<Source> {
    component(project)?;
    component(id)?;
    for hash in [commit, tree] {
        if !matches!(hash.len(), 40 | 64) || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(io::Error::other("workspace: неверный git hash"));
        }
    }
    let workspace = format!("{project}--{id}");
    if workspace.len() > 200 {
        return Err(io::Error::other("слишком длинное имя workspace"));
    }
    Ok(Source {
        project: project.into(),
        commit: commit.into(),
        tree: tree.into(),
        workspace,
    })
}
pub fn validate(source: &Source) -> io::Result<()> {
    let id = source
        .workspace
        .strip_prefix(&format!("{}--", source.project))
        .ok_or_else(|| io::Error::other("неверное имя workspace"))?;
    if self::source(&source.project, id, &source.commit, &source.tree)? != *source {
        return Err(io::Error::other("неверная workspace"));
    }
    Ok(())
}
fn git(root: &Path, args: &[&str]) -> io::Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .stdin(Stdio::null())
        .output()?;
    if !out.status.success() {
        return Err(io::Error::other(
            String::from_utf8_lossy(&out.stderr).trim(),
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().into())
}
fn no_link(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_symlink() => Err(io::Error::other(format!(
            "workspace: символическая ссылка {}",
            path.display()
        ))),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}
pub fn execute(home: &Path, request: Request) -> io::Result<Value> {
    validate(&request.source)?;
    let parent = home.join("ferry");
    let root = parent.join(&request.source.workspace);
    no_link(&parent)?;
    no_link(&root)?;
    no_link(&root.join(".git"))?;
    let marker = root.join(".git/ferry-workspace.json");
    let claims = home.join(".ferry/workspace-claims");
    let _guard = lock(
        &home.join(".ferry"),
        &format!("claim-{}", request.source.workspace),
    )?;
    let claim = claims.join(format!("{}.json", request.source.workspace));
    if marker.exists() {
        let saved: Source = serde_json::from_slice(&fs::read(&marker)?)?;
        if saved != request.source {
            return Err(io::Error::other("workspace id уже связан с другим снимком"));
        }
        return Ok(json!({"source":saved,"cwd":root,"ready":true,"reused":true}));
    }
    let claimed = if claim.exists() {
        let saved: Source = serde_json::from_slice(&fs::read(&claim)?)?;
        if saved != request.source {
            return Err(io::Error::other("workspace id уже занят другим снимком"));
        }
        true
    } else {
        false
    };
    if root.exists() && !claimed {
        return Err(io::Error::other(
            "каталог workspace уже существует без метки ferry; он не изменён",
        ));
    }
    if request.action == Action::Status {
        return Ok(json!({"source":request.source,"cwd":root,"ready":false}));
    }
    if request.action == Action::Prepare {
        if !claimed {
            fs::create_dir_all(&parent)?;
            fs::create_dir(&root)?;
            fs::create_dir_all(&claims)?;
            crate::write_private(&claim, &serde_json::to_string(&request.source)?)?;
        } else {
            fs::create_dir_all(&root)?;
        }
        return Ok(json!({"source":request.source,"cwd":root,"ready":false}));
    }
    if !claimed {
        return Err(io::Error::other("workspace не зарезервирована"));
    }
    if git(&root, &["rev-parse", "HEAD"])? != request.source.commit
        || git(&root, &["rev-parse", "HEAD^{tree}"])? != request.source.tree
    {
        return Err(io::Error::other("workspace содержит другой снимок"));
    }
    crate::write_private(&marker, &serde_json::to_string(&request.source)?)?;
    Ok(json!({"source":request.source,"cwd":root,"ready":true,"reused":false}))
}
pub fn prepare_local(
    home: &Path,
    root: &Path,
    source: &Source,
    extra: &[PathBuf],
) -> io::Result<Value> {
    let state = execute(
        home,
        Request {
            source: source.clone(),
            action: Action::Prepare,
        },
    )?;
    if state["ready"] == true {
        return Ok(state);
    }
    let dest = home.join("ferry").join(&source.workspace);
    fs::create_dir_all(&dest)?;
    if !dest.join(".git").exists() {
        git(&dest, &["init", "-q"])?;
    }
    git(&dest, &["config", "core.autocrlf", "false"])?;
    git(&dest, &["config", "core.longpaths", "true"])?;
    let root = fs::canonicalize(root)?;
    git(
        &dest,
        &["fetch", "-q", "--", &root.to_string_lossy(), &source.commit],
    )?;
    git(&dest, &["checkout", "-q", "--detach", &source.commit])?;
    for relative in extra {
        if relative.as_os_str().is_empty()
            || relative
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err(io::Error::other("files: только пути внутри проекта"));
        }
        if relative.components().any(|c| c.as_os_str() == ".git") {
            return Err(io::Error::other("files: .git не копируется"));
        }
        let mut target = dest.clone();
        for component in relative.components() {
            target.push(component.as_os_str());
            no_link(&target)?;
        }
        let from = root.join(relative);
        if from.exists() {
            copy_extra(&from, &dest.join(relative), 0)?;
        }
    }
    execute(
        home,
        Request {
            source: source.clone(),
            action: Action::Seal,
        },
    )
}
fn copy_extra(from: &Path, to: &Path, depth: u32) -> io::Result<()> {
    if depth > 32 {
        return Err(io::Error::other("слишком глубокий каталог files"));
    }
    no_link(to)?;
    let meta = fs::symlink_metadata(from)?;
    if meta.is_symlink() {
        return Err(io::Error::other(
            "files: символические ссылки не копируются",
        ));
    }
    if meta.is_file() {
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(from, to)?;
    } else if meta.is_dir() {
        fs::create_dir_all(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            copy_extra(&entry.path(), &to.join(entry.file_name()), depth + 1)?;
        }
    } else {
        return Err(io::Error::other("files: нужен файл или каталог"));
    }
    Ok(())
}
pub fn verify(cwd: &Path, source: &Source) -> io::Result<()> {
    validate(source)?;
    let repo = crate::sync::repo_at(cwd)?
        .ok_or_else(|| io::Error::other("workspace не является git репозиторием"))?;
    let stored: Source =
        serde_json::from_slice(&fs::read(repo.git_dir.join("ferry-workspace.json"))?)?;
    if stored != *source || git(&repo.root, &["rev-parse", "HEAD"])? != source.commit {
        return Err(io::Error::other(
            "workspace не совпадает со снимком запроса",
        ));
    }
    Ok(())
}
pub fn lock(state: &Path, workspace: &str) -> io::Result<fs::File> {
    if workspace.contains(['/', '\\']) || workspace.contains("..") {
        return Err(io::Error::other("неверный workspace"));
    }
    let dir = state.join("workspace-locks");
    fs::create_dir_all(&dir)?;
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join(workspace))?;
    file.lock()?;
    Ok(file)
}
fn git_input(root: &Path, args: &[&str], bytes: &[u8]) -> io::Result<std::process::Output> {
    use std::io::Write;
    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().unwrap();
    std::thread::scope(|scope| {
        let sent = scope.spawn(move || stdin.write_all(bytes));
        let output = child.wait_with_output();
        let written = sent
            .join()
            .map_err(|_| io::Error::other("поток патча прерван"))?;
        if output.as_ref().is_ok_and(|o| o.status.success()) {
            written?;
        }
        output
    })
}
pub fn patch(root: &Path, patch: &Path, apply: Option<&str>) -> io::Result<Value> {
    let repo = crate::sync::repo_at(root)?
        .ok_or_else(|| io::Error::other("patch: нужен git репозиторий"))?;
    let apply_guard = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(repo.git_dir.join("ferry-patch.lock"))?;
    apply_guard.lock()?;
    let bytes = fs::read(patch)?;
    if bytes.len() > 64 * 1024 * 1024 {
        return Err(io::Error::other("патч больше 64 МиБ"));
    }
    let (_, tree) = crate::sync::snapshot(&repo)?;
    let hash = git_input(&repo.root, &["hash-object", "--stdin"], &bytes)?;
    if !hash.status.success() {
        return Err(io::Error::other("не удалось вычислить hash патча"));
    }
    let token = format!("{}-{tree}", String::from_utf8_lossy(&hash.stdout).trim());
    if apply.is_some_and(|expected| expected != token) {
        return Err(io::Error::other(
            "preview устарел: рабочая копия или патч изменились",
        ));
    }
    let checked = git_input(&repo.root, &["apply", "--check", "--binary", "-"], &bytes)?;
    let clean = bytes.is_empty() || checked.status.success();
    if apply.is_some() && !clean {
        return Err(io::Error::other(format!(
            "патч конфликтует: {}",
            String::from_utf8_lossy(&checked.stderr).trim()
        )));
    }
    let applied = if apply.is_some() && !bytes.is_empty() {
        let out = git_input(&repo.root, &["apply", "--binary", "-"], &bytes)?;
        if !out.status.success() {
            return Err(io::Error::other(
                String::from_utf8_lossy(&out.stderr).trim(),
            ));
        }
        true
    } else {
        false
    };
    let stat = git_input(&repo.root, &["apply", "--stat", "-"], &bytes)?;
    Ok(
        json!({"patch":patch,"preview_token":token,"clean":clean,"conflict":String::from_utf8_lossy(&checked.stderr).trim(),"stat":String::from_utf8_lossy(&stat.stdout).trim(),"applied":applied,"bytes":bytes.len()}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn isolated_copy_is_reused_without_reset_and_patch_requires_fresh_preview() {
        let base =
            std::env::temp_dir().join(format!("ferry-workspace-{}", crate::jobs::unique_id()));
        let root = base.join("original");
        let home = base.join("home");
        fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q"]).unwrap();
        fs::write(root.join("a"), "base\n").unwrap();
        git(&root, &["add", "a"]).unwrap();
        git(
            &root,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                "base",
            ],
        )
        .unwrap();
        let repo = crate::sync::repo_at(&root).unwrap().unwrap();
        let (commit, tree) = crate::sync::snapshot(&repo).unwrap();
        let source = source("project", "one", &commit, &tree).unwrap();
        let prepared = prepare_local(&home, &root, &source, &[]).unwrap();
        assert_eq!(prepared["ready"], true);
        let remote = home.join("ferry").join(&source.workspace);
        fs::write(remote.join("a"), "remote\n").unwrap();
        assert_eq!(
            prepare_local(&home, &root, &source, &[]).unwrap()["reused"],
            true
        );
        assert_eq!(fs::read_to_string(remote.join("a")).unwrap(), "remote\n");
        assert_eq!(fs::read_to_string(root.join("a")).unwrap(), "base\n");
        let data = Command::new("git")
            .arg("-C")
            .arg(&remote)
            .args(["diff", "--binary"])
            .output()
            .unwrap()
            .stdout;
        let file = base.join("changes.patch");
        fs::write(&file, data).unwrap();
        let preview = patch(&root, &file, None).unwrap();
        assert_eq!(preview["clean"], true);
        fs::write(root.join("other"), "local new file\n").unwrap();
        assert!(
            patch(&root, &file, preview["preview_token"].as_str())
                .unwrap_err()
                .to_string()
                .contains("устарел")
        );
        let preview = patch(&root, &file, None).unwrap();
        assert_eq!(
            patch(&root, &file, preview["preview_token"].as_str()).unwrap()["applied"],
            true
        );
        assert_eq!(fs::read_to_string(root.join("a")).unwrap(), "remote\n");
        assert!(self::source("../escape", "id", &commit, &tree).is_err());
        fs::remove_dir_all(base).unwrap();
    }
}

#[cfg(all(test, unix))]
mod collision_tests {
    use super::*;
    #[test]
    fn existing_directories_and_links_are_never_claimed() {
        let home = std::env::temp_dir().join(format!(
            "ferry-workspace-collision-{}",
            crate::jobs::unique_id()
        ));
        let source = source("project", "one", &"a".repeat(40), &"b".repeat(40)).unwrap();
        let dest = home.join("ferry").join(&source.workspace);
        fs::create_dir_all(&dest).unwrap();
        fs::write(dest.join("mine"), "keep").unwrap();
        let request = Request {
            source,
            action: Action::Prepare,
        };
        assert!(execute(&home, request.clone()).is_err());
        assert_eq!(fs::read_to_string(dest.join("mine")).unwrap(), "keep");
        fs::remove_dir_all(&dest).unwrap();
        let outside = home.join("outside");
        fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, &dest).unwrap();
        assert!(execute(&home, request).is_err());
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
        fs::remove_dir_all(home).unwrap();
    }
}
