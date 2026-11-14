use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use thiserror::Error;

use crate::config::{AgentConfig, Settings, ToolName};
use crate::container::SharedRuntime;
use crate::parser::resolve_workspace_path;

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("tool `{0}` is not permitted for this agent")]
    NotPermitted(String),
    #[error("{0}")]
    Other(#[from] anyhow::Error),
}

pub struct ToolRegistry {
    runtime: SharedRuntime,
    settings: Settings,
    /// Optional callback for delegate_to_agent (set by orchestrator).
    pub delegate: Option<DelegateFn>,
}

pub type DelegateFn = Arc<
    dyn Fn(String, String) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send>>
        + Send
        + Sync,
>;

impl ToolRegistry {
    pub fn new(runtime: SharedRuntime, settings: Settings) -> Self {
        Self {
            runtime,
            settings,
            delegate: None,
        }
    }

    pub fn schemas_for(&self, agent: &AgentConfig) -> Vec<Value> {
        agent
            .tools
            .iter()
            .map(|t| tool_schema(t))
            .collect()
    }

    pub fn permitted(agent: &AgentConfig, name: &str) -> bool {
        agent.tools.iter().any(|t| t.as_str() == name)
    }

    pub async fn execute(
        &self,
        agent: &AgentConfig,
        name: &str,
        args: &Value,
    ) -> Result<String, ToolError> {
        if !Self::permitted(agent, name) {
            return Err(ToolError::NotPermitted(name.to_string()));
        }
        let tool = ToolName::parse(name)
            .ok_or_else(|| ToolError::Other(anyhow::anyhow!("unknown tool {name}")))?;

        match tool {
            ToolName::BashExec => self.bash_exec(agent, args).await,
            ToolName::FileRead => self.file_read(agent, args).await,
            ToolName::FileWrite => self.file_write(agent, args).await,
            ToolName::FilePatch => self.file_patch(agent, args).await,
            ToolName::GrepSearch => self.grep_search(agent, args).await,
            ToolName::WebFetch => self.web_fetch(agent, args).await,
            ToolName::WebSearch => self.web_search(agent, args).await,
            ToolName::DelegateToAgent => self.delegate_to_agent(args).await,
        }
        .map_err(ToolError::from)
    }

    async fn bash_exec(&self, agent: &AgentConfig, args: &Value) -> Result<String> {
        let command = args
            .get("command")
            .and_then(|v| v.as_str())
            .context("bash_exec requires command")?;
        let result = self
            .runtime
            .exec(
                &agent.name,
                vec!["bash".into(), "-lc".into(), command.into()],
                self.settings.exec_timeout_sec,
            )
            .await?;
        Ok(format!(
            "exit_code={}\nstdout:\n{}\nstderr:\n{}",
            result.exit_code, result.stdout, result.stderr
        ))
    }

    /// Normalize model-supplied paths: allow `agents/foo`, `./x`, and `/workspace/x`.
    fn normalize_rel_path(path: &str) -> Result<String> {
        let mut p = path.trim().to_string();
        if p.is_empty() || p == "." || p == "./" || p == "/workspace" || p == "/workspace/" {
            return Ok(".".into());
        }
        for prefix in ["/workspace/", "/workspace", "workspace/"] {
            if let Some(rest) = p.strip_prefix(prefix) {
                p = rest.trim_start_matches('/').to_string();
                break;
            }
        }
        if Path::new(&p).is_absolute() {
            bail!("use workspace-relative paths (e.g. agents/coder/soul.md), not {path}");
        }
        if p.is_empty() {
            p = ".".into();
        }
        Ok(p)
    }

    fn host_path(&self, path: &str) -> Result<PathBuf> {
        let rel = Self::normalize_rel_path(path)?;
        resolve_workspace_path(&self.settings.workspace_dir, &rel)
            .map_err(|e| anyhow::anyhow!(e))
    }

    fn container_path(path: &str) -> Result<String> {
        let rel = Self::normalize_rel_path(path)?;
        if rel == "." {
            return Ok("/workspace".into());
        }
        Ok(format!("/workspace/{rel}"))
    }

    async fn file_read(&self, agent: &AgentConfig, args: &Value) -> Result<String> {
        let path = args.get("path").and_then(|v| v.as_str()).context("path")?;
        let host = self.host_path(path)?;
        let cpath = Self::container_path(path)?;

        // Directories: list entries (don't `cat` a dir).
        if host.is_dir() {
            let result = self
                .runtime
                .exec(
                    &agent.name,
                    vec![
                        "bash".into(),
                        "-lc".into(),
                        format!("ls -la -- {cpath:?}"),
                    ],
                    self.settings.exec_timeout_sec,
                )
                .await?;
            if result.exit_code != 0 {
                bail!("file_read (dir) failed: {}", result.stderr);
            }
            return Ok(format!("directory {path}\n{}", result.stdout));
        }

        let result = self
            .runtime
            .exec(
                &agent.name,
                vec!["bash".into(), "-lc".into(), format!("cat -- {cpath:?}")],
                self.settings.exec_timeout_sec,
            )
            .await?;
        if result.exit_code != 0 {
            bail!("file_read failed: {}", result.stderr);
        }
        Ok(result.stdout)
    }

    async fn file_write(&self, agent: &AgentConfig, args: &Value) -> Result<String> {
        if agent.workspace_mode == crate::config::WorkspaceMode::Ro {
            bail!("agent has read-only workspace");
        }
        let path = args.get("path").and_then(|v| v.as_str()).context("path")?;
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .context("content")?;
        let _ = self.host_path(path)?;
        let cpath = Self::container_path(path)?;
        // Write via python to avoid shell escaping hell
        let b64 = base64_encode(content.as_bytes());
        let script = format!(
            "python3 -c \"import base64,pathlib;p=pathlib.Path({cpath:?});p.parent.mkdir(parents=True,exist_ok=True);p.write_bytes(base64.b64decode('{b64}'))\""
        );
        let result = self
            .runtime
            .exec(
                &agent.name,
                vec!["bash".into(), "-lc".into(), script],
                self.settings.exec_timeout_sec,
            )
            .await?;
        if result.exit_code != 0 {
            bail!("file_write failed: {}", result.stderr);
        }
        Ok(format!("wrote {path} ({} bytes)", content.len()))
    }

    async fn file_patch(&self, agent: &AgentConfig, args: &Value) -> Result<String> {
        if agent.workspace_mode == crate::config::WorkspaceMode::Ro {
            bail!("agent has read-only workspace");
        }
        let path = args.get("path").and_then(|v| v.as_str()).context("path")?;
        let diff = args.get("diff").and_then(|v| v.as_str()).context("diff")?;
        let _ = self.host_path(path)?;
        // Apply unified diff with `patch` if available; else rewrite via simple replace protocol
        let cpath = Self::container_path(path)?;
        let b64 = base64_encode(diff.as_bytes());
        let script = format!(
            "python3 - <<'PY'\nimport base64,subprocess,tempfile,os\ndiff=base64.b64decode('{b64}')\nfd,tmp=tempfile.mkstemp()\nos.write(fd,diff);os.close(fd)\nr=subprocess.run(['patch','-p0','--forward',{cpath:?}],input=open(tmp,'rb').read(),capture_output=True)\nos.unlink(tmp)\nprint(r.stdout.decode(),end='')\nprint(r.stderr.decode(),end='')\nraise SystemExit(r.returncode)\nPY"
        );
        let result = self
            .runtime
            .exec(
                &agent.name,
                vec!["bash".into(), "-lc".into(), script],
                self.settings.exec_timeout_sec,
            )
            .await?;
        Ok(format!(
            "exit_code={}\n{}",
            result.exit_code,
            if result.stdout.is_empty() {
                result.stderr
            } else {
                result.stdout
            }
        ))
    }

    async fn grep_search(&self, agent: &AgentConfig, args: &Value) -> Result<String> {
        let pattern = args
            .get("pattern")
            .and_then(|v| v.as_str())
            .context("pattern")?;
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let path = if path.is_empty() { "." } else { path };
        let cpath = Self::container_path(path).unwrap_or_else(|_| "/workspace".into());
        let cmd = format!("rg --line-number --color never -e {pattern:?} {cpath:?} || true");
        let result = self
            .runtime
            .exec(
                &agent.name,
                vec!["bash".into(), "-lc".into(), cmd],
                self.settings.exec_timeout_sec,
            )
            .await?;
        Ok(result.stdout)
    }

    async fn web_fetch(&self, agent: &AgentConfig, args: &Value) -> Result<String> {
        if !agent.network {
            bail!("agent network profile disabled");
        }
        let url = args.get("url").and_then(|v| v.as_str()).context("url")?;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()?;
        let resp = client.get(url).send().await?.error_for_status()?;
        let text = resp.text().await?;
        let truncated = if text.len() > 50_000 {
            format!("{}… [truncated]", &text[..50_000])
        } else {
            text
        };
        Ok(truncated)
    }

    async fn web_search(&self, agent: &AgentConfig, args: &Value) -> Result<String> {
        if !agent.network {
            bail!("agent network profile disabled");
        }
        let query = args.get("query").and_then(|v| v.as_str()).context("query")?;
        // Lightweight DuckDuckGo instant-answer style fetch (no API key).
        let url = format!(
            "https://duckduckgo.com/html/?q={}",
            urlencoding_encode(query)
        );
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .user_agent("stubot/0.1")
            .build()?;
        let text = client.get(&url).send().await?.text().await?;
        let snippet: String = text.chars().take(8_000).collect();
        Ok(format!("search_query={query}\nhtml_snippet=\n{snippet}"))
    }

    async fn delegate_to_agent(&self, args: &Value) -> Result<String> {
        let agent_name = args
            .get("agent_name")
            .and_then(|v| v.as_str())
            .context("agent_name")?
            .to_string();
        let task = args
            .get("task")
            .and_then(|v| v.as_str())
            .context("task")?
            .to_string();
        let Some(delegate) = &self.delegate else {
            bail!("delegate_to_agent is not wired");
        };
        delegate(agent_name, task).await
    }
}

fn tool_schema(tool: &ToolName) -> Value {
    let (name, description, properties, required) = match tool {
        ToolName::BashExec => (
            "bash_exec",
            "Execute a shell command inside the agent container",
            json!({"command": {"type": "string"}}),
            json!(["command"]),
        ),
        ToolName::FileRead => (
            "file_read",
            "Read a file or list a directory. Path is workspace-relative (e.g. agents/coder/soul.md or agents). /workspace/... is also accepted.",
            json!({"path": {"type": "string", "description": "Workspace-relative path"}}),
            json!(["path"]),
        ),
        ToolName::FileWrite => (
            "file_write",
            "Write content to a file under /workspace",
            json!({"path": {"type": "string"}, "content": {"type": "string"}}),
            json!(["path", "content"]),
        ),
        ToolName::FilePatch => (
            "file_patch",
            "Apply a unified diff to a file under /workspace",
            json!({"path": {"type": "string"}, "diff": {"type": "string"}}),
            json!(["path", "diff"]),
        ),
        ToolName::GrepSearch => (
            "grep_search",
            "Search code with ripgrep inside the container",
            json!({"pattern": {"type": "string"}, "path": {"type": "string"}}),
            json!(["pattern"]),
        ),
        ToolName::WebFetch => (
            "web_fetch",
            "Fetch a URL over HTTP",
            json!({"url": {"type": "string"}}),
            json!(["url"]),
        ),
        ToolName::WebSearch => (
            "web_search",
            "Search the web for a query",
            json!({"query": {"type": "string"}}),
            json!(["query"]),
        ),
        ToolName::DelegateToAgent => (
            "delegate_to_agent",
            "Invoke another agent and await its response",
            json!({"agent_name": {"type": "string"}, "task": {"type": "string"}}),
            json!(["agent_name", "task"]),
        ),
    };

    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": {
                "type": "object",
                "properties": properties,
                "required": required
            }
        }
    })
}

fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let mut n = 0u32;
        for (i, b) in chunk.iter().enumerate() {
            n |= (*b as u32) << (16 - 8 * i);
        }
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn urlencoding_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ToolName, WorkspaceMode};
    use crate::container::NullRuntime;

    fn agent_with(tools: Vec<ToolName>) -> AgentConfig {
        AgentConfig {
            name: "coder".into(),
            color: "cyan".into(),
            image: "stubot-base:latest".into(),
            workspace_mode: WorkspaceMode::Rw,
            network: false,
            tools,
            model: None,
            dir: PathBuf::from("agents/coder"),
            is_default: false,
        }
    }

    #[tokio::test]
    async fn enforces_tool_permissions() {
        let settings = Settings {
            llm_base_url: String::new(),
            llm_api_key: "x".into(),
            default_model: "m".into(),
            workspace_dir: PathBuf::from("."),
            docker_network: "n".into(),
            container_prefix: "p".into(),
            exec_timeout_sec: 5,
            agents_dir: PathBuf::from("agents"),
            skip_containers: true,
        };
        let reg = ToolRegistry::new(Arc::new(NullRuntime), settings);
        let agent = agent_with(vec![ToolName::FileRead]);
        let err = reg
            .execute(&agent, "bash_exec", &json!({"command": "ls"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotPermitted(_)));
    }

    #[test]
    fn schemas_only_for_assigned_tools() {
        let settings = Settings {
            llm_base_url: String::new(),
            llm_api_key: "x".into(),
            default_model: "m".into(),
            workspace_dir: PathBuf::from("."),
            docker_network: "n".into(),
            container_prefix: "p".into(),
            exec_timeout_sec: 5,
            agents_dir: PathBuf::from("agents"),
            skip_containers: true,
        };
        let reg = ToolRegistry::new(Arc::new(NullRuntime), settings);
        let agent = agent_with(vec![ToolName::GrepSearch, ToolName::FileRead]);
        let schemas = reg.schemas_for(&agent);
        assert_eq!(schemas.len(), 2);
        let names: Vec<_> = schemas
            .iter()
            .filter_map(|s| s["function"]["name"].as_str())
            .collect();
        assert!(names.contains(&"grep_search"));
        assert!(names.contains(&"file_read"));
        assert!(!names.contains(&"web_fetch"));
    }
}

#[allow(dead_code)]
fn _path_hint(_: &Path) {}
