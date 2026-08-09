//! Настройки проекта — на хабе, в `~/.ferry/projects/<проект>.toml`; в репозиторий не кладутся.
//!
//! ```toml
//! tools = ["cmake", "ninja"]   # doctor проверит их на узлах
//! files = [".env"]             # sync возит их на узел мимо git
//! ```
//!
//! Файла нет — оба списка пустые.

use std::fs;
use std::io;

#[derive(Debug, Default, PartialEq)]
pub struct Project {
    pub tools: Vec<String>,
    pub files: Vec<String>,
}

pub fn load(name: &str) -> io::Result<Project> {
    let path = crate::ferry_home()?
        .join("projects")
        .join(format!("{name}.toml"));
    match fs::read_to_string(&path) {
        Ok(text) => parse(&text).map_err(|e| io::Error::other(format!("{}: {e}", path.display()))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Project::default()),
        Err(e) => Err(e),
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
            _ => return Err(format!("неизвестный ключ {key}")),
        }
        rest = after.trim_start();
    }
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
}
