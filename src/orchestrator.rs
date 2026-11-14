use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::Mutex;

use crate::config::{AgentsFile, Settings};
use crate::container::SharedRuntime;
use crate::llm::{ChatMessage, LlmClient, TokenUsage};
use crate::parser::ParsedInput;
use crate::tools::{DelegateFn, ToolRegistry};
use crate::ui;

const MAX_HISTORY_MESSAGES: usize = 40;

pub struct Orchestrator {
    pub settings: Settings,
    pub agents: AgentsFile,
    pub runtime: SharedRuntime,
    pub llm: Arc<LlmClient>,
    pub tools: Arc<Mutex<ToolRegistry>>,
    pub session_usage: Arc<Mutex<TokenUsage>>,
    /// Multi-turn chat for the REPL (user/assistant only).
    pub chat_history: Arc<Mutex<Vec<ChatMessage>>>,
}

impl Orchestrator {
    pub async fn bootstrap(&self) -> Result<()> {
        self.runtime.ensure_network().await?;
        for name in self.agents.names() {
            let agent = self.agents.get(&name)?;
            match self.runtime.ensure_agent(agent).await {
                Ok(cname) => ui::status(
                    &agent.color,
                    &agent.name,
                    &format!("ready ({cname}) · {}", agent.dir.display()),
                ),
                Err(e) => ui::warn(&format!("@{name} container failed: {e}")),
            }
        }
        let this = self.as_delegate_handle();
        {
            let mut tools = self.tools.lock().await;
            tools.delegate = Some(this);
        }
        Ok(())
    }

    fn as_delegate_handle(&self) -> DelegateFn {
        let settings = self.settings.clone();
        let agents = self.agents.clone();
        let runtime = self.runtime.clone();
        let llm = self.llm.clone();
        let tools = self.tools.clone();
        let session_usage = self.session_usage.clone();
        let chat_history = self.chat_history.clone();
        Arc::new(move |agent_name: String, task: String| {
            let settings = settings.clone();
            let agents = agents.clone();
            let runtime = runtime.clone();
            let llm = llm.clone();
            let tools = tools.clone();
            let session_usage = session_usage.clone();
            let chat_history = chat_history.clone();
            Box::pin(async move {
                let orch = Orchestrator {
                    settings,
                    agents,
                    runtime,
                    llm,
                    tools,
                    session_usage,
                    chat_history,
                };
                // Delegations are one-shot (no REPL history bleed).
                orch.run_agent(&agent_name, &task, "", false).await
            })
        })
    }

    pub async fn handle_parsed(&self, parsed: ParsedInput) -> Result<String> {
        let context = parsed.context_blocks();
        let mut last = String::new();
        let multi = parsed.agents.len() > 1;
        for (i, agent_name) in parsed.agents.iter().enumerate() {
            let task = if i == 0 {
                parsed.prompt.clone()
            } else {
                format!(
                    "Previous agent output:\n{last}\n\nContinue the user task:\n{}",
                    parsed.prompt
                )
            };
            // Keep REPL history for the first (or only) agent on a turn.
            let use_history = i == 0 && !multi;
            last = self
                .run_agent(agent_name, &task, &context, use_history)
                .await?;
        }
        Ok(last)
    }

    pub async fn run_agent(
        &self,
        agent_name: &str,
        task: &str,
        file_context: &str,
        use_history: bool,
    ) -> Result<String> {
        let agent = self.agents.get(agent_name)?.clone();
        let _ = self.runtime.ensure_agent(&agent).await;
        ui::banner(&agent.color, &agent.name, "thinking…");

        let roster = self.agents.names().join(", ");
        let mut system_prompt = agent
            .build_system_prompt()
            .with_context(|| format!("loading soul/memory for @{agent_name}"))?;
        system_prompt.push_str(&format!(
            "\n# Roster\nAvailable agents (folders under agents/): {roster}\n\
             Paths for tools are workspace-relative (e.g. agents/coder/soul.md). \
             To list a folder, file_read that path.\n"
        ));

        let mut user_content = task.to_string();
        if !file_context.is_empty() {
            user_content.push_str("\n\n## Attached workspace context\n");
            user_content.push_str(file_context);
        }

        let mut messages = vec![ChatMessage {
            role: "system".into(),
            content: Some(system_prompt),
            name: None,
            tool_calls: None,
            tool_call_id: None,
        }];

        if use_history {
            let history = self.chat_history.lock().await;
            messages.extend(history.iter().cloned());
        }

        messages.push(ChatMessage {
            role: "user".into(),
            content: Some(user_content.clone()),
            name: None,
            tool_calls: None,
            tool_call_id: None,
        });

        let tools = self.tools.lock().await;
        let outcome = self
            .llm
            .chat_stream_loop(&agent, &mut messages, &tools, 12)
            .await
            .with_context(|| format!("agent @{agent_name} LLM loop"))?;
        drop(tools);

        if use_history {
            let mut history = self.chat_history.lock().await;
            history.push(ChatMessage {
                role: "user".into(),
                content: Some(user_content),
                name: None,
                tool_calls: None,
                tool_call_id: None,
            });
            history.push(ChatMessage {
                role: "assistant".into(),
                content: Some(outcome.text.clone()),
                name: None,
                tool_calls: None,
                tool_call_id: None,
            });
            while history.len() > MAX_HISTORY_MESSAGES {
                history.remove(0);
            }
        }

        {
            let mut session = self.session_usage.lock().await;
            session.absorb(&outcome.usage);
            ui::panel_reply(&agent.color, &agent.name, &outcome.text);
            ui::token_usage(&agent.color, &agent.name, &outcome.usage, Some(&session));
        }

        Ok(outcome.text)
    }

    pub async fn reset_chat(&self) {
        self.chat_history.lock().await.clear();
    }

    pub async fn session_tokens(&self) -> TokenUsage {
        self.session_usage.lock().await.clone()
    }

    pub async fn shutdown(&self) -> Result<()> {
        if !self.settings.skip_containers {
            self.runtime.shutdown_all().await?;
        }
        Ok(())
    }
}
