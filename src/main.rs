use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use owo_colors::OwoColorize;
use tokio::sync::Mutex;
use tracing_subscriber::EnvFilter;

use stubot::config::{AgentsFile, Settings};
use stubot::container::build_runtime;
use stubot::llm::{LlmClient, TokenUsage};
use stubot::orchestrator::Orchestrator;
use stubot::parser::parse_input;
use stubot::tools::ToolRegistry;
use stubot::ui;

#[derive(Debug, Parser)]
#[command(name = "stubot", about = "Multi-agent terminal orchestrator")]
struct Cli {
    /// Workspace root mounted into agent containers
    #[arg(long, env = "WORKSPACE_DIR")]
    workspace: Option<PathBuf>,

    /// Directory of agent folders (each with agent.yaml, soul.md, memory.md)
    #[arg(long, default_value = "agents", env = "AGENTS_DIR")]
    agents: PathBuf,

    /// Skip Docker lifecycle (tools run against null runtime)
    #[arg(long)]
    skip_containers: bool,

    /// Keep containers after exit
    #[arg(long)]
    keep_containers: bool,

    /// One-shot prompt (non-interactive)
    #[arg(long)]
    prompt: Option<String>,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("stubot=info".parse()?))
        .with_target(false)
        .init();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async_main())
}

async fn async_main() -> Result<()> {
    let cli = Cli::parse();
    let settings =
        Settings::from_env(cli.workspace.clone(), Some(cli.agents.clone()), cli.skip_containers)?;
    let agents = AgentsFile::load_dir(&settings.agents_dir)
        .with_context(|| format!("load agents from {}", settings.agents_dir.display()))?;

    let runtime = build_runtime(&settings)?;
    let llm = Arc::new(LlmClient::new(settings.clone()));
    let tools = Arc::new(Mutex::new(ToolRegistry::new(
        runtime.clone(),
        settings.clone(),
    )));

    let orch = Orchestrator {
        settings: settings.clone(),
        agents: agents.clone(),
        runtime: runtime.clone(),
        llm,
        tools,
        session_usage: Arc::new(Mutex::new(TokenUsage::default())),
        chat_history: Arc::new(Mutex::new(Vec::new())),
    };

    println!("{}", "Stubot".bold());
    println!(
        "workspace={} agents={} model={} containers={}",
        settings.workspace_dir.display(),
        settings.agents_dir.display(),
        settings.default_model,
        if settings.skip_containers {
            "skipped"
        } else {
            "docker"
        }
    );

    if let Err(e) = orch.bootstrap().await {
        ui::warn(&format!("bootstrap: {e}"));
    }

    let result = if let Some(prompt) = cli.prompt {
        run_once(&orch, &prompt).await
    } else {
        run_repl(&orch).await
    };

    if !cli.keep_containers {
        let _ = orch.shutdown().await;
    }

    result
}

async fn run_once(orch: &Orchestrator, prompt: &str) -> Result<()> {
    let parsed = parse_input(
        prompt,
        &orch.settings.workspace_dir,
        &orch.agents.default_agent,
    );
    let _ = orch.handle_parsed(parsed).await?;
    Ok(())
}

async fn run_repl(orch: &Orchestrator) -> Result<()> {
    ui::print_help();
    let mut rl = ui::new_editor(orch.agents.names(), orch.settings.workspace_dir.clone())?;

    loop {
        let Some(line) = ui::read_line(&mut rl)? else {
            break;
        };
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('/') {
            match line.as_str() {
                "/help" | "/h" => ui::print_help(),
                "/exit" | "/quit" => break,
                "/clear" => print!("\x1B[2J\x1B[H"),
                "/reset" => {
                    orch.reset_chat().await;
                    println!("chat history cleared");
                }
                "/tokens" => {
                    let s = orch.session_tokens().await;
                    println!(
                        "session tokens  prompt={}  completion={}  total={}",
                        s.prompt_tokens, s.completion_tokens, s.total_tokens
                    );
                }
                "/agents" => {
                    for name in orch.agents.names() {
                        let a = orch.agents.get(&name)?;
                        let mark = if name == orch.agents.default_agent {
                            "*"
                        } else {
                            " "
                        };
                        println!(
                            "{mark} {}  color={} tools={}  {}",
                            ui::badge(&a.color, &a.name),
                            a.color,
                            a.tools
                                .iter()
                                .map(|t| t.as_str())
                                .collect::<Vec<_>>()
                                .join(","),
                            a.dir.display()
                        );
                    }
                }
                "/containers" => match orch.runtime.list_agent_containers().await {
                    Ok(list) if list.is_empty() => println!("(no stubot containers)"),
                    Ok(list) => {
                        for (n, s) in list {
                            println!("{n}\t{s}");
                        }
                    }
                    Err(e) => ui::warn(&format!("{e}")),
                },
                other => ui::warn(&format!("unknown command: {other}")),
            }
            continue;
        }

        let parsed = parse_input(
            &line,
            &orch.settings.workspace_dir,
            &orch.agents.default_agent,
        );
        if let Err(e) = orch.handle_parsed(parsed).await {
            ui::warn(&format!("{e:#}"));
        }
    }

    ui::save_history(&mut rl);
    Ok(())
}
