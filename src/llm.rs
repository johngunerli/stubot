use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use futures_util::StreamExt;
use owo_colors::OwoColorize;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::{AgentConfig, Settings};
use crate::tools::ToolRegistry;
use crate::ui;

#[derive(Debug, Clone, Default)]
pub struct TokenUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    pub reasoning_tokens: u64,
}

impl TokenUsage {
    pub fn from_json(v: &Value) -> Option<Self> {
        let u = v.get("usage")?;
        let prompt = u.get("prompt_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
        let completion = u
            .get("completion_tokens")
            .and_then(|x| x.as_u64())
            .unwrap_or(0);
        let total = u
            .get("total_tokens")
            .and_then(|x| x.as_u64())
            .unwrap_or(prompt + completion);
        let reasoning = u
            .pointer("/completion_tokens_details/reasoning_tokens")
            .and_then(|x| x.as_u64())
            .or_else(|| u.get("reasoning_tokens").and_then(|x| x.as_u64()))
            .unwrap_or(0);
        if prompt == 0 && completion == 0 && total == 0 && reasoning == 0 {
            return None;
        }
        Some(Self {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: total,
            reasoning_tokens: reasoning,
        })
    }

    pub fn absorb(&mut self, other: &TokenUsage) {
        self.prompt_tokens += other.prompt_tokens;
        self.completion_tokens += other.completion_tokens;
        self.total_tokens += other.total_tokens;
        self.reasoning_tokens += other.reasoning_tokens;
    }
}

#[derive(Debug, Clone)]
pub struct ChatOutcome {
    pub text: String,
    pub usage: TokenUsage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub type_: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Clone)]
pub struct LlmClient {
    http: Client,
    settings: Settings,
}

impl LlmClient {
    pub fn new(settings: Settings) -> Self {
        Self {
            http: Client::new(),
            settings,
        }
    }

    fn model_for(&self, agent: &AgentConfig) -> String {
        agent
            .model
            .clone()
            .unwrap_or_else(|| self.settings.default_model.clone())
    }

    fn endpoint(&self) -> String {
        let base = self.settings.llm_base_url.trim_end_matches('/');
        format!("{base}/chat/completions")
    }

    pub async fn chat_stream_loop(
        &self,
        agent: &AgentConfig,
        messages: &mut Vec<ChatMessage>,
        tools: &ToolRegistry,
        max_iters: usize,
    ) -> Result<ChatOutcome> {
        let tool_schemas = tools.schemas_for(agent);
        let mut final_text = String::new();
        let mut usage_total = TokenUsage::default();

        for _ in 0..max_iters {
            let mut body = json!({
                "model": self.model_for(agent),
                "messages": messages,
                "stream": true,
                "stream_options": { "include_usage": true },
                // Groq gpt-oss: expose thinking in delta.reasoning
                "include_reasoning": true,
            });
            if let Ok(effort) = std::env::var("REASONING_EFFORT") {
                if !effort.is_empty() {
                    body["reasoning_effort"] = json!(effort);
                }
            }
            if !tool_schemas.is_empty() {
                body["tools"] = Value::Array(tool_schemas.clone());
                body["tool_choice"] = json!("auto");
            }

            let resp = self
                .http
                .post(self.endpoint())
                .bearer_auth(&self.settings.llm_api_key)
                .header("Content-Type", "application/json")
                .json(&body)
                .send()
                .await
                .context("LLM request failed")?;

            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                return Err(anyhow!("LLM error {status}: {text}"));
            }

            let mut stream = resp.bytes_stream();
            let mut assistant_content = String::new();
            let mut tool_calls: Vec<ToolCallAccum> = Vec::new();
            let mut buf = String::new();
            let mut turn_usage = TokenUsage::default();
            let mut thinking_started = false;
            let mut answer_started = false;

            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                buf.push_str(&String::from_utf8_lossy(&chunk));
                while let Some(pos) = buf.find('\n') {
                    let line = buf[..pos].trim_end().to_string();
                    buf = buf[pos + 1..].to_string();
                    if line.is_empty() || !line.starts_with("data:") {
                        continue;
                    }
                    let data = line[5..].trim();
                    if data == "[DONE]" {
                        continue;
                    }
                    let Ok(v) = serde_json::from_str::<Value>(data) else {
                        continue;
                    };
                    if let Some(u) = TokenUsage::from_json(&v) {
                        turn_usage = u;
                    }
                    let Some(choice) = v["choices"].as_array().and_then(|a| a.first()) else {
                        continue;
                    };
                    let delta = &choice["delta"];

                    // Groq gpt-oss → `reasoning`; other providers → `reasoning_content`
                    let reasoning = delta["reasoning"]
                        .as_str()
                        .or_else(|| delta["reasoning_content"].as_str());
                    if let Some(r) = reasoning {
                        if !r.is_empty() {
                            ui::stream_thinking(
                                &agent.color,
                                &agent.name,
                                r,
                                &mut thinking_started,
                            );
                        }
                    }

                    if let Some(c) = delta["content"].as_str() {
                        if !c.is_empty() {
                            if thinking_started && !answer_started {
                                ui::thinking_done(true);
                                thinking_started = false;
                                println!(
                                    "{} {}",
                                    ui::badge(&agent.color, &agent.name),
                                    "answer".dimmed()
                                );
                            }
                            answer_started = true;
                            assistant_content.push_str(c);
                            ui::stream_token(&agent.color, &agent.name, c);
                        }
                    }
                    if let Some(tcs) = delta["tool_calls"].as_array() {
                        if thinking_started {
                            ui::thinking_done(true);
                            thinking_started = false;
                        }
                        for tc in tcs {
                            let idx = tc["index"].as_u64().unwrap_or(0) as usize;
                            while tool_calls.len() <= idx {
                                tool_calls.push(ToolCallAccum::default());
                            }
                            let acc = &mut tool_calls[idx];
                            if let Some(id) = tc["id"].as_str() {
                                acc.id = id.to_string();
                            }
                            if let Some(name) = tc["function"]["name"].as_str() {
                                acc.name.push_str(name);
                            }
                            if let Some(args) = tc["function"]["arguments"].as_str() {
                                acc.arguments.push_str(args);
                            }
                        }
                    }
                }
            }

            if thinking_started {
                ui::thinking_done(true);
            }
            usage_total.absorb(&turn_usage);

            if !tool_calls.is_empty() {
                let calls: Vec<ToolCall> = tool_calls
                    .into_iter()
                    .filter(|t| !t.name.is_empty())
                    .map(|t| ToolCall {
                        id: if t.id.is_empty() {
                            format!("call_{}", t.name)
                        } else {
                            t.id
                        },
                        type_: "function".into(),
                        function: FunctionCall {
                            name: t.name,
                            arguments: t.arguments,
                        },
                    })
                    .collect();

                messages.push(ChatMessage {
                    role: "assistant".into(),
                    content: if assistant_content.is_empty() {
                        None
                    } else {
                        Some(assistant_content.clone())
                    },
                    name: None,
                    tool_calls: Some(calls.clone()),
                    tool_call_id: None,
                });

                for call in calls {
                    ui::tool_log(
                        &agent.color,
                        &agent.name,
                        &call.function.name,
                        &call.function.arguments,
                    );
                    let args: Value = serde_json::from_str(&call.function.arguments)
                        .unwrap_or_else(|_| json!({}));
                    let result = match tools.execute(agent, &call.function.name, &args).await {
                        Ok(s) => s,
                        Err(e) => format!("tool error: {e}"),
                    };
                    ui::tool_result(&agent.color, &agent.name, &result);
                    messages.push(ChatMessage {
                        role: "tool".into(),
                        content: Some(result),
                        name: None,
                        tool_calls: None,
                        tool_call_id: Some(call.id),
                    });
                }
                continue;
            }

            final_text = assistant_content;
            messages.push(ChatMessage {
                role: "assistant".into(),
                content: Some(final_text.clone()),
                name: None,
                tool_calls: None,
                tool_call_id: None,
            });
            break;
        }

        Ok(ChatOutcome {
            text: final_text,
            usage: usage_total,
        })
    }
}

#[derive(Default)]
struct ToolCallAccum {
    id: String,
    name: String,
    arguments: String,
}

pub type SharedLlm = Arc<LlmClient>;
