# Stubot

Local-first multi-agent terminal orchestrator. Each agent is a **folder** you can hand-author; each runs in its own Docker container.

## Quick start

```bash
cp .env.example .env   # set LLM_API_KEY (Groq)
./install.sh
./scripts/build-images.sh
stubot
```

```bash
stubot --prompt "@coder hi"
stubot --skip-containers
```

## Agent folders

```text
agents/
  coordinator/
    agent.yaml    # color, tools, image, network, model, default
    soul.md       # personality / instructions
    memory.md     # lasting notes (edit by hand anytime)
  coder/
    ...
```

Add a new agent by creating `agents/<name>/` with those three files. Stubot discovers folders on startup.

## Tokens

After each reply Stubot prints turn + session token counts. Use `/tokens` in the REPL.

## Slash commands

`/help` `/agents` `/tokens` `/containers` `/clear` `/exit`
