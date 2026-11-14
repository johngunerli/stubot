use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkspaceMode {
    Ro,
    Rw,
}

impl WorkspaceMode {
    pub fn as_bind_mode(&self) -> &str {
        match self {
            Self::Ro => "ro",
            Self::Rw => "rw",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolName {
    BashExec,
    FileRead,
    FileWrite,
    FilePatch,
    GrepSearch,
    WebFetch,
    WebSearch,
    DelegateToAgent,
}

impl ToolName {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::BashExec => "bash_exec",
            Self::FileRead => "file_read",
            Self::FileWrite => "file_write",
            Self::FilePatch => "file_patch",
            Self::GrepSearch => "grep_search",
            Self::WebFetch => "web_fetch",
            Self::WebSearch => "web_search",
            Self::DelegateToAgent => "delegate_to_agent",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "bash_exec" => Some(Self::BashExec),
            "file_read" => Some(Self::FileRead),
            "file_write" => Some(Self::FileWrite),
            "file_patch" => Some(Self::FilePatch),
            "grep_search" => Some(Self::GrepSearch),
            "web_fetch" => Some(Self::WebFetch),
            "web_search" => Some(Self::WebSearch),
            "delegate_to_agent" => Some(Self::DelegateToAgent),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct AgentYaml {
    #[serde(default = "default_color")]
    color: String,
    #[serde(default = "default_image")]
    image: String,
    #[serde(default = "default_workspace_mode")]
    workspace_mode: WorkspaceMode,
    #[serde(default)]
    network: bool,
    #[serde(default)]
    tools: Vec<ToolName>,
    model: Option<String>,
    #[serde(default)]
    default: bool,
}

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub name: String,
    pub color: String,
    pub image: String,
    pub workspace_mode: WorkspaceMode,
    pub network: bool,
    pub tools: Vec<ToolName>,
    pub model: Option<String>,
    pub dir: PathBuf,
    pub is_default: bool,
}

impl AgentConfig {
    pub fn soul_path(&self) -> PathBuf {
        self.dir.join("soul.md")
    }

    pub fn memory_path(&self) -> PathBuf {
        self.dir.join("memory.md")
    }

    /// Reload soul.md + memory.md into a system prompt (hand-editable on disk).
    pub fn build_system_prompt(&self) -> Result<String> {
        let soul = read_optional_md(&self.soul_path())?;
        let memory = read_optional_md(&self.memory_path())?;
        let mut out = String::new();
        out.push_str(&format!(
            "You are @{}. Follow your soul. Use memory for continuity.\n\n",
            self.name
        ));
        out.push_str("# Soul\n\n");
        out.push_str(if soul.trim().is_empty() {
            "(no soul.md yet — add one in this agent's folder)\n"
        } else {
            soul.trim()
        });
        out.push_str("\n\n# Memory\n\n");
        out.push_str(if memory.trim().is_empty() {
            "(empty — update memory.md in this agent's folder as needed)\n"
        } else {
            memory.trim()
        });
        out.push('\n');
        Ok(out)
    }
}

fn read_optional_md(path: &Path) -> Result<String> {
    if !path.exists() {
        return Ok(String::new());
    }
    fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
}

fn default_color() -> String {
    "white".into()
}
fn default_image() -> String {
    "stubot-base:latest".into()
}
fn default_workspace_mode() -> WorkspaceMode {
    WorkspaceMode::Rw
}

#[derive(Debug, Clone)]
pub struct AgentsFile {
    pub default_agent: String,
    pub agents: HashMap<String, AgentConfig>,
    pub root: PathBuf,
}

impl AgentsFile {
    /// Discover agents by scanning `agents/<name>/agent.yaml` (+ soul.md / memory.md).
    pub fn load_dir(root: &Path) -> Result<Self> {
        let root = if root.is_absolute() {
            root.to_path_buf()
        } else {
            std::env::current_dir()?.join(root)
        };

        if !root.is_dir() {
            bail!(
                "agents directory not found: {} (create agents/<name>/ with agent.yaml, soul.md, memory.md)",
                root.display()
            );
        }

        let mut agents = HashMap::new();
        let mut default_agent: Option<String> = None;

        for entry in fs::read_dir(&root).with_context(|| format!("reading {}", root.display()))? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let dir = entry.path();
            let yaml_path = dir.join("agent.yaml");
            if !yaml_path.exists() {
                // Allow hand-created folders without yaml yet — skip with a soft note via tracing later
                continue;
            }
            let text = fs::read_to_string(&yaml_path)
                .with_context(|| format!("reading {}", yaml_path.display()))?;
            let raw: AgentYaml = serde_yaml::from_str(&text)
                .with_context(|| format!("parsing {}", yaml_path.display()))?;

            if raw.default {
                default_agent = Some(name.clone());
            }

            agents.insert(
                name.clone(),
                AgentConfig {
                    name: name.clone(),
                    color: raw.color,
                    image: raw.image,
                    workspace_mode: raw.workspace_mode,
                    network: raw.network,
                    tools: raw.tools,
                    model: raw.model,
                    dir,
                    is_default: raw.default,
                },
            );
        }

        if agents.is_empty() {
            bail!(
                "no agents found under {} — add a folder like agents/coder/ with agent.yaml",
                root.display()
            );
        }

        let default_agent = default_agent
            .or_else(|| {
                if agents.contains_key("coordinator") {
                    Some("coordinator".into())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| {
                let mut names: Vec<_> = agents.keys().cloned().collect();
                names.sort();
                names[0].clone()
            });

        Ok(Self {
            default_agent,
            agents,
            root,
        })
    }

    pub fn get(&self, name: &str) -> Result<&AgentConfig> {
        let key = name.trim_start_matches('@');
        self.agents
            .get(key)
            .with_context(|| format!("unknown agent @{key} (add agents/{key}/)"))
    }

    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<_> = self.agents.keys().cloned().collect();
        names.sort();
        names
    }
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub llm_base_url: String,
    pub llm_api_key: String,
    pub default_model: String,
    pub workspace_dir: PathBuf,
    pub docker_network: String,
    pub container_prefix: String,
    pub exec_timeout_sec: u64,
    pub agents_dir: PathBuf,
    pub skip_containers: bool,
}

impl Settings {
    pub fn from_env(
        workspace: Option<PathBuf>,
        agents_dir: Option<PathBuf>,
        skip: bool,
    ) -> Result<Self> {
        let _ = dotenvy::dotenv();
        let workspace_dir = workspace
            .or_else(|| std::env::var("WORKSPACE_DIR").ok().map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("."))
            .canonicalize()
            .unwrap_or_else(|_| {
                PathBuf::from(".")
                    .canonicalize()
                    .unwrap_or_else(|_| PathBuf::from("."))
            });

        let agents_dir = agents_dir.unwrap_or_else(|| {
            std::env::var("AGENTS_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("agents"))
        });

        let llm_api_key = std::env::var("LLM_API_KEY").unwrap_or_default();
        if llm_api_key.is_empty() {
            bail!("LLM_API_KEY is required (set in .env)");
        }

        Ok(Self {
            llm_base_url: std::env::var("LLM_BASE_URL")
                .unwrap_or_else(|_| "https://api.groq.com/openai/v1".into()),
            llm_api_key,
            default_model: std::env::var("DEFAULT_MODEL")
                .unwrap_or_else(|_| "llama-3.3-70b-versatile".into()),
            workspace_dir,
            docker_network: std::env::var("DOCKER_NETWORK")
                .unwrap_or_else(|_| "stubot_net".into()),
            container_prefix: std::env::var("CONTAINER_PREFIX")
                .unwrap_or_else(|_| "stubot-agent-".into()),
            exec_timeout_sec: std::env::var("EXEC_TIMEOUT_SEC")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(120),
            agents_dir,
            skip_containers: skip,
        })
    }

    pub fn container_name(&self, agent: &str) -> String {
        format!("{}{}", self.container_prefix, agent)
    }
}
