use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    pub label: String,
    pub file_type: String,
    pub source_file: String,
    pub source_location: Option<String>,
    #[serde(rename = "type")]
    pub kind: String,
    pub language: String,
    #[serde(rename = "_origin")]
    pub origin: String,
    /// Explicitly limited semantic projection; absent for ordinary AST nodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extraction_coverage: Option<String>,
}

pub fn id(parts: &[&str]) -> String {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update((part.len() as u64).to_le_bytes());
        hash.update(part.as_bytes());
    }
    format!("gre_{:x}", hash.finalize())
}

impl Node {
    pub fn new(file: &str, kind: &str, name: &str, language: &str, line: usize) -> Self {
        Self {
            id: id(&[file, kind, name]),
            label: name.to_owned(),
            file_type: if language == "html" {
                "document"
            } else {
                "code"
            }
            .into(),
            source_file: file.into(),
            source_location: (line > 0).then(|| format!("L{line}")),
            kind: kind.into(),
            language: language.into(),
            origin: "ast".into(),
            extraction_coverage: None,
        }
    }
    pub fn file(file: &str, language: &str) -> Self {
        Self::new(file, "file", file, language, 0)
    }
}

#[derive(Debug, Serialize)]
pub struct Edge<'a> {
    pub source: &'a str,
    pub target: &'a str,
    pub relation: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_relation: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<&'a str>,
    pub confidence: &'a str,
    pub confidence_score: f64,
    pub weight: f64,
    pub source_file: &'a str,
    pub source_location: Option<String>,
    #[serde(rename = "_origin")]
    pub origin: &'a str,
}
