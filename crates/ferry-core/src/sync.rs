//! Проект на узле: снимок рабочего дерева — с незакоммиченным и новыми файлами — коммитом вне
//! веток, и проверка правок, сделанных на узле поверх его последнего снимка. Проект `…/<имя>`
//! живёт на узле в `~/ferry/<имя>`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::sh_quote;

pub struct Repo {
    pub root: PathBuf,
    pub git_dir: PathBuf,
    /// Имя каталога репозитория — оно же каталог на узле: `~/ferry/<name>`.
    pub name: String,
    /// Где мы внутри репозитория: `src-tauri/` или пусто.
    pub prefix: String,
}

/// Репозиторий, в котором текущий каталог.
pub fn repo() -> io::Result<Option<Repo>> {
    repo_at(Path::new("."))
}

pub fn repo_at(dir: &Path) -> io::Result<Option<Repo>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "rev-parse",
            "--show-toplevel",
            "--absolute-git-dir",
            "--show-prefix",
        ])
        .stderr(Stdio::null())
        .output()?;
    if !out.status.success() {
        return Ok(None);
    }
    let out = String::from_utf8_lossy(&out.stdout);
    let mut lines = out.lines();
    let (Some(root), Some(git_dir)) = (lines.next(), lines.next()) else {
        return Ok(None);
    };
    let root = PathBuf::from(root);
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(Some(Repo {
        name,
        root,
        git_dir: git_dir.into(),
        prefix: lines.next().unwrap_or_default().into(),
    }))
}

pub fn snapshot_lock(git_dir: &Path) -> io::Result<fs::File> {
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(git_dir.join("ferry-snapshot.lock"))?;
    file.lock()?;
    Ok(file)
}

/// Снимок рабочего дерева: коммит без родителя и вне веток и его дерево. Автор и даты
/// фиксированы, поэтому то же дерево даёт тот же хеш; без родителя на узел едет только дерево,
/// а не вся история.
pub fn snapshot(repo: &Repo) -> io::Result<(String, String)> {
    let _lock = snapshot_lock(&repo.git_dir)?;
    let index = repo.git_dir.join("ferry-index");
    // Копия индекса: git переиспользует его stat-кэш и не перечитывает всё дерево.
    if fs::copy(repo.git_dir.join("index"), &index).is_err() {
        let _ = fs::remove_file(&index);
    }
    let git = || {
        let mut git = Command::new("git");
        git.arg("-C").arg(&repo.root).env("GIT_INDEX_FILE", &index);
        git
    };
    run(git().args(["add", "-A"]))?;
    let tree = run(git().arg("write-tree"))?;
    let mut commit = git();
    commit.args(["commit-tree", &tree, "-m", "ferry snapshot"]);
    for who in ["AUTHOR", "COMMITTER"] {
        commit
            .env(format!("GIT_{who}_NAME"), "ferry")
            .env(format!("GIT_{who}_EMAIL"), "ferry@localhost")
            .env(format!("GIT_{who}_DATE"), "@0 +0000");
    }
    Ok((run(&mut commit)?, tree))
}

/// Скрипт для шелла узла, из домашнего каталога: заводит репозиторий `ferry/<name>`, если его
/// нет, и печатает патч правок поверх последнего снимка — или ничего, если правок нет или они
/// уже совпадают с деревом `keep` (новым снимком).
pub fn changes_script(name: &str, keep: &str) -> String {
    let dir = sh_quote(&format!("ferry/{name}"));
    let keep = sh_quote(keep);
    // autocrlf=false — файлы на узле байт в байт как здесь; longpaths — для node_modules.
    format!(
        "set -e
mkdir -p ferry
[ -d {dir}/.git ] || git init -q {dir}
cd {dir}
git config core.autocrlf false
git config core.longpaths true
git rev-parse -q --verify HEAD >/dev/null || exit 0
export GIT_INDEX_FILE=.git/ferry-index
cp .git/index $GIT_INDEX_FILE
git add -A
t=$(git write-tree)
[ $t = $(git rev-parse 'HEAD^{{tree}}') ] || [ $t = {keep} ] || git diff --cached --binary HEAD"
    )
}

/// Запустить git, вернуть stdout без перевода строки; при ошибке — его stderr.
fn run(command: &mut Command) -> io::Result<String> {
    let out = command.stderr(Stdio::piped()).output()?;
    if !out.status.success() {
        return Err(io::Error::other(format!(
            "git: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Снимок берёт незакоммиченное и новые файлы, детерминирован и не трогает ветку и индекс.
    #[test]
    fn snapshot_takes_the_worktree_and_leaves_the_repo_alone() {
        let root = std::env::temp_dir().join(format!("ferry-snapshot-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}: {out:?}");
            String::from_utf8(out.stdout).unwrap().trim().to_string()
        };
        git(&["init", "-q"]);
        fs::write(root.join("a.txt"), "1\n").unwrap();
        fs::write(root.join(".gitignore"), "ignored\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "init"]);
        let head = git(&["rev-parse", "HEAD"]);

        fs::write(root.join("a.txt"), "2\n").unwrap();
        fs::write(root.join("new.txt"), "new\n").unwrap();
        fs::write(root.join("ignored"), "x\n").unwrap();
        let repo = Repo {
            git_dir: PathBuf::from(git(&["rev-parse", "--absolute-git-dir"])),
            root: root.clone(),
            name: "r".into(),
            prefix: String::new(),
        };
        let (commit, tree) = snapshot(&repo).unwrap();

        assert_eq!(snapshot(&repo).unwrap(), (commit.clone(), tree.clone()));
        assert_eq!(git(&["show", &format!("{commit}:a.txt")]), "2");
        assert_eq!(git(&["show", &format!("{commit}:new.txt")]), "new");
        // Без родителя: на узел едет только дерево, а не история.
        assert!(
            git(&["rev-list", "--parents", "-n1", &commit])
                .split_whitespace()
                .count()
                == 1
        );
        assert!(
            git(&["ls-tree", "--name-only", &tree])
                .lines()
                .all(|f| f != "ignored")
        );
        assert_eq!(git(&["rev-parse", "HEAD"]), head);
        assert_eq!(git(&["diff", "--cached", "--name-only"]), "");
        assert_eq!(
            git(&["ls-files", "--others", "--exclude-standard"]),
            "new.txt"
        );
        let _ = fs::remove_dir_all(&root);
    }
}
