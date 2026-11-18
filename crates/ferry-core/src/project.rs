//! Настройки проекта — на хабе, в `~/.ferry/projects/<проект>.toml`; в репозиторий не кладутся.
//!
//! ```toml
//! tools = ["cmake", "ninja"]   # doctor проверит их на узлах
//! files = [".env"]             # sync возит их на узел мимо git
//! caches = ["build", "dist"]   # игнорируемые git каталоги, доступные для storage preview
//! ```
//!
//! Файла нет — списки пустые.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, PartialEq)]
pub struct Project {
    pub tools: Vec<String>,
    pub files: Vec<String>,
    pub caches: Vec<String>,
}

const BINDING: &str = "ferry-project";

pub(crate) fn relative_cache_path(path: &str) -> io::Result<PathBuf> {
    if path.is_empty()
        || path.len() > 512
        || path.contains(['\\', ':', '<', '>', '"', '|', '?', '*'])
        || path.chars().any(char::is_control)
        || path.split('/').any(|part| {
            part.is_empty()
                || matches!(part, "." | "..")
                || part.eq_ignore_ascii_case(".git")
                || part.ends_with(['.', ' '])
        })
    {
        return Err(io::Error::other(
            "caches: нужен относительный путь внутри проекта без .git и переходов",
        ));
    }
    Ok(PathBuf::from(path))
}

pub(crate) fn paths_intersect(left: &str, right: &str) -> bool {
    let left = left.to_lowercase();
    let right = right.replace('\\', "/").to_lowercase();
    left == right
        || left.starts_with(&format!("{right}/"))
        || right.starts_with(&format!("{left}/"))
}

pub fn validate_caches(paths: &[String], files: &[String]) -> io::Result<()> {
    if paths.len() > 32 || files.len() > 256 {
        return Err(io::Error::other(
            "caches: максимум 32 каталога и 256 защищённых files",
        ));
    }
    for (index, path) in paths.iter().enumerate() {
        relative_cache_path(path)?;
        if paths[..index]
            .iter()
            .any(|other| paths_intersect(path, other))
        {
            return Err(io::Error::other(
                "caches: каталоги не должны повторяться или включать друг друга",
            ));
        }
        if files.iter().any(|file| paths_intersect(path, file)) {
            return Err(io::Error::other(
                "caches: каталог пересекается с защищённым files проекта",
            ));
        }
    }
    for file in files {
        relative_cache_path(&file.replace('\\', "/"))?;
    }
    Ok(())
}

fn profile_path(dir: &Path, name: &str) -> io::Result<PathBuf> {
    if name.is_empty()
        || matches!(name, "." | "..")
        || name.contains(['/', '\\', ':'])
        || name.chars().any(char::is_control)
    {
        return Err(io::Error::other("неверное имя профиля проекта"));
    }
    Ok(dir.join(format!("{name}.toml")))
}

fn read_profile(path: &Path) -> io::Result<Project> {
    let text = fs::read_to_string(path)
        .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))?;
    parse(&text).map_err(|e| io::Error::other(format!("{}: {e}", path.display())))
}

fn resolve(
    dir: &Path,
    git_dir: Option<&Path>,
    default: &str,
) -> io::Result<(Option<PathBuf>, Project)> {
    let bound = git_dir
        .map(|d| fs::read_to_string(d.join(BINDING)))
        .transpose();
    match bound {
        Ok(Some(name)) => {
            let path = profile_path(dir, &name)?;
            let profile = read_profile(&path)?;
            return Ok((Some(path), profile));
        }
        Ok(None) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let path = profile_path(dir, default)?;
    match read_profile(&path) {
        Ok(profile) => Ok((Some(path), profile)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok((None, Project::default())),
        Err(e) => Err(e),
    }
}

pub fn for_checkout(repo: &crate::sync::Repo) -> io::Result<(Option<PathBuf>, Project)> {
    resolve(
        &crate::ferry_home()?.join("projects"),
        Some(&repo.git_dir),
        &repo.name,
    )
}

pub fn for_repo(root: &Path, default: &str) -> io::Result<(Option<PathBuf>, Project)> {
    let repo = crate::sync::repo_at(root)?;
    resolve(
        &crate::ferry_home()?.join("projects"),
        repo.as_ref()
            .and_then(|r| (r.root == root).then_some(r.git_dir.as_path())),
        default,
    )
}

pub fn bind(repo: &crate::sync::Repo, name: Option<&str>) -> io::Result<()> {
    let binding = repo.git_dir.join(BINDING);
    if let Some(name) = name {
        read_profile(&profile_path(&crate::ferry_home()?.join("projects"), name)?)?;
        return crate::write_private(&binding, name);
    }
    match fs::remove_file(binding) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

// ponytail: понимаем только `ключ = ["строка", …]` (массив можно на нескольких строках) и
// комментарии `#`; понадобится больше — крейт toml.
fn parse(text: &str) -> Result<Project, String> {
    let mut clean = String::new();
    for line in text.lines() {
        let mut in_string = false;
        for c in line.chars() {
            match c {
                '"' => in_string = !in_string,
                '#' if !in_string => break,
                _ => {}
            }
            clean.push(c);
        }
        clean.push('\n');
    }

    let mut project = Project::default();
    let mut rest = clean.trim_start();
    while !rest.is_empty() {
        let (key, value) = rest.split_once('=').ok_or("ожидалось `ключ = [...]`")?;
        let key = key.trim();
        let value = value
            .trim_start()
            .strip_prefix('[')
            .ok_or(format!("{key}: ожидался массив [...]"))?;
        let (items, after) = value
            .split_once(']')
            .ok_or(format!("{key}: нет закрывающей ]"))?;
        let items = items
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(|item| {
                item.strip_prefix('"')
                    .and_then(|item| item.strip_suffix('"'))
                    .map(String::from)
                    .ok_or(format!("{key}: элементы — строки в кавычках, а не {item}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        match key {
            "tools" => project.tools = items,
            "files" => project.files = items,
            "caches" => project.caches = items,
            _ => return Err(format!("неизвестный ключ {key}")),
        }
        rest = after.trim_start();
    }
    validate_caches(&project.caches, &project.files).map_err(|e| e.to_string())?;
    Ok(project)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_documented_format() {
        let text = "# проект\ntools = [\"cmake\", \"ninja\"]  # для doctor\n\nfiles = [\n  \".env\",\n  \"a#b\",\n]\n";
        assert_eq!(
            parse(text),
            Ok(Project {
                tools: vec!["cmake".into(), "ninja".into()],
                files: vec![".env".into(), "a#b".into()],
                caches: vec![],
            })
        );
        assert_eq!(parse(""), Ok(Project::default()));
    }

    #[test]
    fn rejects_what_it_does_not_understand() {
        assert!(parse("tools = cmake").is_err());
        assert!(parse("tools = [cmake]").is_err());
        assert!(parse("tool = [\"cmake\"]").is_err());
        assert!(parse("tools = [\"cmake\"").is_err());
    }

    #[test]
    fn caches_are_relative_disjoint_and_never_include_extra_files() {
        let profile =
            parse("files = [\".env\"]\ncaches = [\"build\", \"frontend/.cache\"]").unwrap();
        assert_eq!(profile.caches, ["build", "frontend/.cache"]);
        for path in [
            "", ".", "..", "../build", "/build", "C:/build", "a\\b", ".git", "a/.GIT/x", "build/",
            "build.", "build ",
        ] {
            assert!(validate_caches(&[path.into()], &[]).is_err(), "{path}");
        }
        for file in ["build/.env", "BUILD", "build"] {
            assert!(validate_caches(&["build".into()], &[file.into()]).is_err());
        }
        assert!(validate_caches(&["build/cache".into()], &["build".into()]).is_err());
        assert!(validate_caches(&["build".into(), "build/cache".into()], &[]).is_err());
        assert!(validate_caches(&["build".into(), "BUILD".into()], &[]).is_err());
        assert!(validate_caches(&["build".into()], &["builder/.env".into()]).is_ok());
    }

    #[test]
    fn binding_is_local_to_a_checkout_and_never_falls_back_if_missing() {
        let temp = std::env::temp_dir().join(format!("ferry-profile-{}", crate::jobs::unique_id()));
        let profiles = temp.join("projects");
        fs::create_dir_all(&profiles).unwrap();
        let path = profiles.join("application.toml");
        fs::write(&path, "tools = [\"cargo\"]\nfiles = [\"fixture.json\"]").unwrap();
        for name in ["checkout-one", "checkout-two"] {
            let git = temp.join(name);
            fs::create_dir(&git).unwrap();
            fs::write(git.join(BINDING), "application").unwrap();
            let (selected, project) = resolve(&profiles, Some(&git), name).unwrap();
            assert_eq!(selected, Some(path.clone()));
            assert_eq!(project.tools, ["cargo"]);
            assert_eq!(project.files, ["fixture.json"]);
        }
        assert_eq!(
            resolve(&profiles, Some(&temp), "checkout-one").unwrap().0,
            None
        );
        fs::write(temp.join(BINDING), "missing").unwrap();
        assert!(resolve(&profiles, Some(&temp), "application").is_err());
        assert!(profile_path(&profiles, "../application").is_err());
        fs::remove_dir_all(temp).unwrap();
    }
}
