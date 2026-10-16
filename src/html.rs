use crate::{Stats, store::Store};
use anyhow::Result;
use scraper::{Html, Selector};

pub fn local_target(file: &str, value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty()
        || value.starts_with('#')
        || value.starts_with("//")
        || value.contains(":")
        || value.contains(['<', '>', '\\'])
    {
        return None;
    }
    let path = value.split(['?', '#']).next()?;
    if path.is_empty() {
        return None;
    }
    // Percent-encoded paths are deliberately unresolved, not incorrectly rewritten.
    if path.contains('%') {
        return None;
    }
    let mut parts: Vec<&str> = if path.starts_with('/') {
        vec![]
    } else {
        file.rsplit_once('/')
            .map(|(parent, _)| parent.split('/').collect())
            .unwrap_or_default()
    };
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            p => parts.push(p),
        }
    }
    Some(parts.join("/"))
}

pub fn extract(
    bytes: &[u8],
    file: &str,
    file_id: &str,
    store: &Store,
    stats: &mut Stats,
) -> Result<()> {
    let text = std::str::from_utf8(bytes)?;
    let document = Html::parse_document(text);
    let selector =
        Selector::parse("a[href], form[action], script[src], link[href], iframe[src], img[src]")
            .unwrap();
    // A base element changes URL interpretation. Avoid fabricating local edges.
    let has_base = document
        .select(&Selector::parse("base[href]").unwrap())
        .next()
        .is_some();
    for element in document.select(&selector) {
        let (attribute, relation) = match element.value().name() {
            "form" => ("action", "submits_to"),
            "a" => ("href", "links_to"),
            "link" => ("href", "uses"),
            _ => ("src", "uses"),
        };
        if let Some(value) = element.value().attr(attribute) {
            if has_base {
                stats.skipped_html_references += 1;
                continue;
            }
            if let Some(target) = local_target(file, value) {
                store.reference(file_id, "file", &target, None, relation, file, 0)?;
            } else {
                stats.skipped_html_references += 1;
            }
        }
    }
    Ok(())
}
