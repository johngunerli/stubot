use std::fs;
use std::path::{Component, Path, PathBuf};

use regex::Regex;
use thiserror::Error;

#[derive(Debug, Clone)]
pub struct FileRef {
    pub raw: String,
    pub resolved: Option<PathBuf>,
    pub content: Option<String>,
    pub error: Option<String>,
    pub is_dir: bool,
    pub listing: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ParsedInput {
    pub raw: String,
    pub prompt: String,
    pub agents: Vec<String>,
    pub files: Vec<FileRef>,
}

impl ParsedInput {
    pub fn context_blocks(&self) -> String {
        self.files
            .iter()
            .map(|ref_| {
                if let Some(err) = &ref_.error {
                    return format!(r#"<file path="{}" error="{}" />"#, ref_.raw, err);
                }
                if ref_.is_dir {
                    let items = ref_.listing.join("\n");
                    return format!(
                        "<directory path=\"{}\">\n{}\n</directory>",
                        ref_.raw, items
                    );
                }
                format!(
                    "<file path=\"{}\">\n{}\n</file>",
                    ref_.raw,
                    ref_.content.as_deref().unwrap_or("")
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

#[derive(Debug, Error)]
pub enum PathResolveError {
    #[error("absolute paths are not allowed: {0}")]
    Absolute(String),
    #[error("path escapes workspace: {0}")]
    EscapesWorkspace(String),
}

fn agent_re() -> Regex {
    // Rust `regex` has no lookaround — use a capturing group for the boundary char.
    Regex::new(r"(?i)(?:^|[^\w])@([A-Za-z][A-Za-z0-9_-]*)").expect("agent regex")
}

fn file_re() -> Regex {
    Regex::new(r"(?:^|[^\w])#([A-Za-z0-9_./~-]+)").expect("file regex")
}

pub fn extract_agents(text: &str) -> Vec<String> {
    let mut seen = Vec::new();
    for cap in agent_re().captures_iter(text) {
        let name = cap[1].to_ascii_lowercase();
        if !seen.contains(&name) {
            seen.push(name);
        }
    }
    seen
}

pub fn extract_file_tokens(text: &str) -> Vec<String> {
    let mut seen = Vec::new();
    for cap in file_re().captures_iter(text) {
        let raw = cap[1].trim_end_matches(['.', ',', ';', ':', '!', '?']);
        if !raw.is_empty() && !seen.iter().any(|s: &String| s == raw) {
            seen.push(raw.to_string());
        }
    }
    seen
}

pub fn strip_tokens(text: &str) -> String {
    let cleaned = agent_re().replace_all(text, |caps: &regex::Captures| {
        let full = caps.get(0).map(|m| m.as_str()).unwrap_or("");
        if full.starts_with('@') {
            " ".to_string()
        } else {
            format!("{} ", &full[..1])
        }
    });
    let cleaned = file_re().replace_all(&cleaned, |caps: &regex::Captures| {
        let full = caps.get(0).map(|m| m.as_str()).unwrap_or("");
        if full.starts_with('#') {
            " ".to_string()
        } else {
            format!("{} ", &full[..1])
        }
    });
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Resolve `raw` under `workspace` without leaving the root (no canonicalize required).
pub fn resolve_workspace_path(workspace: &Path, raw: &str) -> Result<PathBuf, PathResolveError> {
    let candidate = Path::new(raw);
    if candidate.is_absolute() {
        return Err(PathResolveError::Absolute(raw.to_string()));
    }

    let root = if workspace.exists() {
        workspace
            .canonicalize()
            .unwrap_or_else(|_| workspace.to_path_buf())
    } else {
        workspace.to_path_buf()
    };

    let mut normalized = PathBuf::new();
    for comp in candidate.components() {
        match comp {
            Component::Prefix(_) | Component::RootDir => {
                return Err(PathResolveError::Absolute(raw.to_string()));
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(PathResolveError::EscapesWorkspace(raw.to_string()));
                }
            }
            Component::Normal(s) => normalized.push(s),
        }
    }

    Ok(root.join(normalized))
}

pub fn load_file_ref(workspace: &Path, raw: &str, max_bytes: usize) -> FileRef {
    let mut ref_ = FileRef {
        raw: raw.to_string(),
        resolved: None,
        content: None,
        error: None,
        is_dir: false,
        listing: Vec::new(),
    };

    let path = match resolve_workspace_path(workspace, raw) {
        Ok(p) => p,
        Err(e) => {
            ref_.error = Some(e.to_string());
            return ref_;
        }
    };
    ref_.resolved = Some(path.clone());

    if !path.exists() {
        ref_.error = Some(format!("Not found: {raw}"));
        return ref_;
    }

    if path.is_dir() {
        ref_.is_dir = true;
        match fs::read_dir(&path) {
            Ok(rd) => {
                let mut names: Vec<_> = rd
                    .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
                    .collect();
                names.sort();
                names.truncate(200);
                ref_.listing = names;
            }
            Err(e) => ref_.error = Some(format!("Cannot list directory: {e}")),
        }
        return ref_;
    }

    match fs::read(&path) {
        Ok(data) => {
            let truncated = data.len() > max_bytes;
            let slice = if truncated { &data[..max_bytes] } else { &data };
            let mut text = String::from_utf8_lossy(slice).into_owned();
            if truncated {
                text.push_str(&format!("\n\n... [truncated at {max_bytes} bytes]"));
            }
            ref_.content = Some(text);
        }
        Err(e) => ref_.error = Some(format!("Cannot read file: {e}")),
    }
    ref_
}

pub fn parse_input(text: &str, workspace: &Path, default_agent: &str) -> ParsedInput {
    let mut agents = extract_agents(text);
    if agents.is_empty() {
        agents.push(default_agent.to_string());
    }
    let files = extract_file_tokens(text)
        .iter()
        .map(|raw| load_file_ref(workspace, raw, 200_000))
        .collect();
    let mut prompt = strip_tokens(text);
    if prompt.is_empty() {
        prompt = text.trim().to_string();
    }
    ParsedInput {
        raw: text.to_string(),
        prompt,
        agents,
        files,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn extracts_agents_and_files() {
        let p = parse_input(
            "@coder @reviewer refactor #src/main.rs please",
            Path::new("."),
            "coordinator",
        );
        assert_eq!(p.agents, vec!["coder", "reviewer"]);
        assert_eq!(p.files[0].raw, "src/main.rs");
        assert!(p.prompt.contains("refactor"));
        assert!(p.prompt.contains("please"));
        assert!(!p.prompt.contains("@coder"));
    }

    #[test]
    fn defaults_to_coordinator() {
        let p = parse_input("hello", Path::new("."), "coordinator");
        assert_eq!(p.agents, vec!["coordinator"]);
    }

    #[test]
    fn blocks_path_traversal() {
        let dir = tempdir().unwrap();
        let err = resolve_workspace_path(dir.path(), "../etc/passwd").unwrap_err();
        assert!(matches!(err, PathResolveError::EscapesWorkspace(_)));
    }

    #[test]
    fn blocks_absolute_paths() {
        let err = resolve_workspace_path(Path::new("/tmp"), "/etc/passwd").unwrap_err();
        assert!(matches!(err, PathResolveError::Absolute(_)));
    }

    #[test]
    fn loads_file_under_workspace() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("hello.txt");
        fs::write(&file, "hi").unwrap();
        let r = load_file_ref(dir.path(), "hello.txt", 1000);
        assert_eq!(r.content.as_deref(), Some("hi"));
        assert!(r.error.is_none());
    }
}
