use std::borrow::Cow;
use std::path::PathBuf;

use owo_colors::{AnsiColors, DynColors, OwoColorize};
use rustyline::completion::{Completer, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::{CmdKind, Highlighter};
use rustyline::hint::Hinter;
use rustyline::validate::Validator;
use rustyline::{CompletionType, Config, Context, Editor, Helper};
use walkdir::WalkDir;

pub fn color_for(name: &str) -> DynColors {
    let c = match name.to_ascii_lowercase().as_str() {
        "cyan" | "bright_cyan" => AnsiColors::Cyan,
        "magenta" | "bright_magenta" => AnsiColors::Magenta,
        "yellow" | "bright_yellow" => AnsiColors::Yellow,
        "green" | "bright_green" => AnsiColors::Green,
        "blue" => AnsiColors::Blue,
        "bright_blue" => AnsiColors::BrightBlue,
        "red" | "bright_red" => AnsiColors::Red,
        "white" | "bright_white" => AnsiColors::BrightWhite,
        "black" => AnsiColors::Black,
        _ => AnsiColors::White,
    };
    DynColors::Ansi(c)
}

pub fn badge(color: &str, agent: &str) -> String {
    format!("[@{agent}]")
        .color(color_for(color))
        .bold()
        .to_string()
}

pub fn status(color: &str, agent: &str, msg: &str) {
    println!("{} {}", badge(color, agent), msg.dimmed());
}

pub fn warn(msg: &str) {
    eprintln!("{}", format!("! {msg}").color(AnsiColors::Yellow));
}

pub fn banner(color: &str, agent: &str, msg: &str) {
    println!(
        "\n{} {}",
        badge(color, agent),
        msg.color(color_for(color))
    );
}

pub fn stream_token(color: &str, _agent: &str, token: &str) {
    print!("{}", token.color(color_for(color)));
    let _ = std::io::Write::flush(&mut std::io::stdout());
}

/// Reasoning / "thinking" tokens — dimmed so they read as inner monologue.
pub fn stream_thinking(color: &str, agent: &str, token: &str, started: &mut bool) {
    if !*started {
        println!(
            "\n{} {}",
            badge(color, agent),
            "thinking".dimmed().italic()
        );
        *started = true;
    }
    print!("{}", token.dimmed().italic());
    let _ = std::io::Write::flush(&mut std::io::stdout());
}

pub fn thinking_done(started: bool) {
    if started {
        println!();
    }
}

pub fn tool_log(color: &str, agent: &str, name: &str, args: &str) {
    let preview: String = args.chars().take(160).collect();
    println!(
        "\n{} {} {}",
        badge(color, agent),
        format!("⚙ {name}").color(color_for(color)),
        preview.dimmed()
    );
    if name == "delegate_to_agent" {
        println!(
            "{} {}",
            badge(color, agent),
            "handoff — nested agent is running; you'll see its stream next…"
                .dimmed()
                .italic()
        );
    }
}

pub fn tool_result(color: &str, agent: &str, result: &str) {
    let preview: String = result.chars().take(400).collect();
    println!(
        "{} {}\n{}",
        badge(color, agent),
        "tool result".dimmed(),
        preview.dimmed()
    );
}

pub fn panel_reply(color: &str, agent: &str, reply: &str) {
    // Content was already streamed live — just close the turn with a marker.
    println!();
    let bar = "─".repeat(48).color(color_for(color)).to_string();
    println!("{bar}");
    if reply.is_empty() {
        println!("{} (no final text — check tools/thinking above)", badge(color, agent));
    } else {
        println!("{} done", badge(color, agent));
    }
    println!("{bar}");
}

pub fn token_usage(
    color: &str,
    agent: &str,
    turn: &crate::llm::TokenUsage,
    session: Option<&crate::llm::TokenUsage>,
) {
    let mut turn_line = format!(
        "tokens  prompt={}  completion={}  total={}",
        turn.prompt_tokens, turn.completion_tokens, turn.total_tokens
    );
    if turn.reasoning_tokens > 0 {
        turn_line.push_str(&format!("  reasoning={}", turn.reasoning_tokens));
    }
    println!("{} {}", badge(color, agent), turn_line.dimmed());
    if let Some(s) = session {
        let mut sess = format!(
            "session prompt={}  completion={}  total={}",
            s.prompt_tokens, s.completion_tokens, s.total_tokens
        );
        if s.reasoning_tokens > 0 {
            sess.push_str(&format!("  reasoning={}", s.reasoning_tokens));
        }
        println!("{} {}", badge(color, agent), sess.dimmed());
    }
    println!();
}

pub fn print_help() {
    println!(
        "{}",
        r#"
Stubot — multi-agent terminal orchestrator

  @agent   route to an agent (fuzzy complete)
  #path    attach workspace file/dir context

Slash commands:
  /help         this help
  /agents       list agents
  /tokens       session token totals
  /reset        clear chat history
  /containers   list agent containers
  /clear        clear screen
  /exit         quit (tears down containers)

Hand-author agents as folders:
  agents/<name>/agent.yaml
  agents/<name>/soul.md
  agents/<name>/memory.md
"#
        .dimmed()
    );
}

pub struct ReplHelper {
    pub agents: Vec<String>,
    pub workspace: PathBuf,
}

impl ReplHelper {
    fn complete_agents(&self, prefix: &str) -> Vec<Pair> {
        let p = prefix.to_ascii_lowercase();
        self.agents
            .iter()
            .filter(|a| a.to_ascii_lowercase().starts_with(&p) || fuzzy_match(a, &p))
            .map(|a| Pair {
                display: format!("@{a}"),
                replacement: format!("@{a} "),
            })
            .collect()
    }

    fn complete_files(&self, prefix: &str) -> Vec<Pair> {
        let mut out = Vec::new();
        let walker = WalkDir::new(&self.workspace)
            .max_depth(6)
            .into_iter()
            .filter_entry(|e| {
                let name = e.file_name().to_string_lossy();
                !name.starts_with('.') && name != "target" && name != "node_modules"
            });
        for entry in walker.flatten() {
            let path = entry.path();
            let Ok(rel) = path.strip_prefix(&self.workspace) else {
                continue;
            };
            let rel_s = rel.to_string_lossy().replace('\\', "/");
            if rel_s.is_empty() {
                continue;
            }
            if rel_s.starts_with(prefix) || fuzzy_match(&rel_s, prefix) {
                out.push(Pair {
                    display: format!("#{rel_s}"),
                    replacement: format!("#{rel_s} "),
                });
            }
            if out.len() >= 40 {
                break;
            }
        }
        out
    }
}

fn fuzzy_match(candidate: &str, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let mut it = candidate.chars();
    for qc in query.chars() {
        loop {
            match it.next() {
                Some(cc) if cc.eq_ignore_ascii_case(&qc) => break,
                Some(_) => continue,
                None => return false,
            }
        }
    }
    true
}

impl Completer for ReplHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let before = &line[..pos];
        if let Some(at) = before.rfind('@') {
            let token = &before[at + 1..];
            if !token.contains(' ') {
                return Ok((at, self.complete_agents(token)));
            }
        }
        if let Some(hash) = before.rfind('#') {
            let token = &before[hash + 1..];
            if !token.contains(' ') {
                return Ok((hash, self.complete_files(token)));
            }
        }
        if before.ends_with('/') || before.is_empty() {
            let cmds = [
            "/help",
            "/agents",
            "/tokens",
            "/reset",
            "/containers",
            "/clear",
            "/exit",
        ];
            let pairs = cmds
                .iter()
                .filter(|c| c.starts_with(before.trim()))
                .map(|c| Pair {
                    display: (*c).into(),
                    replacement: format!("{c} "),
                })
                .collect();
            return Ok((0, pairs));
        }
        Ok((pos, vec![]))
    }
}

impl Hinter for ReplHelper {
    type Hint = String;
    fn hint(&self, _line: &str, _pos: usize, _ctx: &Context<'_>) -> Option<String> {
        None
    }
}

impl Highlighter for ReplHelper {
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        Cow::Borrowed(line)
    }
    fn highlight_char(&self, _line: &str, _pos: usize, _forced: CmdKind) -> bool {
        false
    }
}

impl Validator for ReplHelper {}
impl Helper for ReplHelper {}

pub fn new_editor(agents: Vec<String>, workspace: PathBuf) -> rustyline::Result<Editor<ReplHelper, rustyline::history::DefaultHistory>> {
    let config = Config::builder()
        .completion_type(CompletionType::List)
        .build();
    let mut rl = Editor::with_config(config)?;
    rl.set_helper(Some(ReplHelper { agents, workspace }));
    let _ = rl.load_history(".stubot_history");
    Ok(rl)
}

pub fn read_line(
    rl: &mut Editor<ReplHelper, rustyline::history::DefaultHistory>,
) -> Result<Option<String>, ReadlineError> {
    match rl.readline("stubot❯ ") {
        Ok(line) => {
            let _ = rl.add_history_entry(line.as_str());
            Ok(Some(line))
        }
        Err(ReadlineError::Interrupted) => Ok(Some(String::new())),
        Err(ReadlineError::Eof) => Ok(None),
        Err(e) => Err(e),
    }
}

pub fn save_history(rl: &mut Editor<ReplHelper, rustyline::history::DefaultHistory>) {
    let _ = rl.save_history(".stubot_history");
}
