//! Structured Accessibility data. Indices belong to one snapshot, never to an OS handle.
use serde::{Deserialize, Serialize};
use std::io;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Matcher {
    pub role: Option<String>,
    pub name: Option<String>,
    pub id: Option<String>,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Locator {
    pub role: Option<String>,
    pub name: Option<String>,
    pub id: Option<String>,
    pub within: Option<Matcher>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Element {
    pub index: u32,
    pub parent: Option<u32>,
    pub role: String,
    pub name: String,
    pub id: String,
    pub rect: Option<(i64, i64, i64, i64)>,
    pub enabled: Option<bool>,
    pub focused: Option<bool>,
    pub checked: Option<bool>,
    pub selected: Option<bool>,
    pub value: Option<String>,
    pub protected: bool,
    #[serde(default)]
    pub name_truncated: bool,
    #[serde(default)]
    pub id_truncated: bool,
    #[serde(default)]
    pub value_truncated: bool,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Tree {
    pub elements: Vec<Element>,
    pub complete: bool,
}
impl Locator {
    pub fn validate(&self) -> io::Result<()> {
        validate_fields(&self.role, &self.name, &self.id)?;
        if let Some(w) = &self.within {
            validate_fields(&w.role, &w.name, &w.id)?;
        }
        Ok(())
    }
    pub fn matches(&self, e: &Element, all: &[Element]) -> bool {
        matches_fields(e, &self.role, &self.name, &self.id)
            && self.within.as_ref().is_none_or(|w| {
                ancestors(e, all)
                    .iter()
                    .any(|p| matches_fields(p, &w.role, &w.name, &w.id))
            })
    }
    pub fn hits<'a>(&self, all: &'a [Element]) -> Vec<&'a Element> {
        all.iter().filter(|e| self.matches(e, all)).collect()
    }
    pub fn unique<'a>(&self, all: &'a [Element]) -> io::Result<&'a Element> {
        match self.hits(all).as_slice() {
            [e] => Ok(e),
            [] => Err(io::Error::other("элемент по локатору не найден")),
            many => Err(io::Error::other(format!(
                "локатор неоднозначен: {} элементов",
                many.len()
            ))),
        }
    }
}
fn validate_fields(
    role: &Option<String>,
    name: &Option<String>,
    id: &Option<String>,
) -> io::Result<()> {
    if [role, name, id].iter().all(|v| v.is_none())
        || [role, name, id]
            .iter()
            .any(|v| v.as_ref().is_some_and(|s| s.is_empty() || s.len() > 512))
    {
        return Err(io::Error::other(
            "локатору нужны role, name или id (1..512 байт)",
        ));
    }
    Ok(())
}
fn matches_fields(
    e: &Element,
    role: &Option<String>,
    name: &Option<String>,
    id: &Option<String>,
) -> bool {
    role.as_ref().is_none_or(|s| s == &e.role)
        && name
            .as_ref()
            .is_none_or(|s| !e.name_truncated && s == &e.name)
        && id.as_ref().is_none_or(|s| !e.id_truncated && s == &e.id)
}
pub fn ancestors<'a>(e: &Element, all: &'a [Element]) -> Vec<&'a Element> {
    let mut out = Vec::new();
    let mut parent = e.parent;
    while let Some(p) = parent {
        let Some(e) = all
            .binary_search_by_key(&p, |e| e.index)
            .ok()
            .and_then(|i| all.get(i))
        else {
            break;
        };
        if out.len() >= 40 || out.iter().any(|old: &&Element| old.index == p) {
            break;
        }
        out.push(e);
        parent = e.parent;
    }
    out
}
pub fn select(tree: Tree, query: Option<&Locator>, subtree: bool) -> io::Result<Tree> {
    let elements = if let Some(query) = query {
        let selected = query.unique(&tree.elements)?;
        let parents = ancestors(selected, &tree.elements);
        tree.elements
            .iter()
            .filter(|e| {
                e.index == selected.index
                    || parents.iter().any(|p| p.index == e.index)
                    || (subtree
                        && ancestors(e, &tree.elements)
                            .iter()
                            .any(|p| p.index == selected.index))
            })
            .cloned()
            .collect::<Vec<_>>()
    } else {
        tree.elements
    };
    let complete = tree.complete && elements.len() <= 500;
    Ok(Tree {
        elements: elements.into_iter().take(500).collect(),
        complete,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    pub fn fixture(index: u32, parent: Option<u32>, role: &str, name: &str) -> Element {
        Element {
            index,
            parent,
            role: role.into(),
            name: name.into(),
            id: String::new(),
            rect: Some((0, 0, 10, 10)),
            enabled: Some(true),
            focused: Some(false),
            checked: None,
            selected: None,
            value: None,
            protected: false,
            name_truncated: false,
            id_truncated: false,
            value_truncated: false,
        }
    }
    #[test]
    fn exact_locators_disambiguate_containers_and_keep_ancestors() {
        let all = vec![
            fixture(0, None, "Window", "App"),
            fixture(1, Some(0), "Group", "Settings"),
            fixture(2, Some(1), "Button", "Save"),
            fixture(3, Some(0), "Button", "Save"),
        ];
        let mut locator = Locator {
            name: Some("Save".into()),
            ..Default::default()
        };
        assert!(locator.unique(&all).is_err());
        locator.within = Some(Matcher {
            name: Some("Settings".into()),
            ..Default::default()
        });
        assert_eq!(locator.unique(&all).unwrap().index, 2);
        let filtered = select(
            Tree {
                elements: all,
                complete: true,
            },
            Some(&locator),
            false,
        )
        .unwrap();
        assert_eq!(
            filtered
                .elements
                .iter()
                .map(|e| e.index)
                .collect::<Vec<_>>(),
            [0, 1, 2]
        );
        assert!(filtered.complete);
        assert!(Locator::default().validate().is_err());
        let mut clipped = fixture(0, None, "Button", "Save");
        clipped.name_truncated = true;
        let name = Locator {
            name: Some("Save".into()),
            ..Default::default()
        };
        assert!(name.hits(&[clipped]).is_empty());
    }
}
