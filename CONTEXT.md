# WarpSOLO — Context

## Project

**WarpSOLO** is a private fork of [Warp](https://warp.dev) that adds **local agent execution** — running AI coding agents on your own machine without cloud dependency.

## Glossary

| Term | Definition |
|------|------------|
| **WarpSOLO** | This fork. Personal-use build with local agent loopback. |
| **Loopback Server** | The sidecar process (`oss_loopback.rs`) that replaces cloud API calls with local execution. Binds `127.0.0.1:random`. |
| **Coordinator** | Logical server within loopback that handles MAA (multi-agent) requests, routes to local or remote workers. |
| **Worker** | Execution unit that runs agent turns, serves SSE events. Can be local or on another machine. Binds `0.0.0.0:port` when enabled. |
| **MAA** | Multi-Agent Architecture — the protobuf-based protocol the Warp UI uses to communicate with agents. |
| **Local Agent** | Agent running on the same machine, using the `genai` crate to call local LLM providers (OpenAI-compatible, Anthropic, Ollama, etc.). |
| **Remote Worker** | A WarpSOLO instance on another machine (LAN or Tailscale) that accepts agent execution requests. |
| **Pairing Token** | Single auth mechanism for worker API. No rotation, no rate limiting. |
| **Shared Session** | Live-view session (`warposs://shared_session/{id}`) created for each agent run, allowing the UI to stream events. |
| **Follow-up** | Subsequent turns in an agent conversation, preserving tool-call context across runs. |
| **Handoff** | Local-to-cloud transition — sending a local agent session to a cloud agent. |
| **llm.toml** | Config file in `~/.warp-oss/` defining LLM providers, models, API keys, system prompts, and tool permissions. |
| **agent-worker.toml** | Config file in `~/.warp-oss/` defining worker listener settings (enabled, bind, port, pairing_token, peers). |
| **local-account.json** | Auto-created stable local user/device identity. |

## Architecture

```
Warp UI (existing)
    │
    ▼
POST /ai/multi-agent (MAA protobuf)
    │
    ▼
Loopback Server (oss_loopback.rs)
├── Coordinator (127.0.0.1:random)
│   ├── /ai/multi-agent          → local agent or proxy to worker
│   ├── /graphql/v2              → GraphQL passthrough
│   ├── /api/v1/agent/*          → agent management
│   ├── /worker/discovered       → discovered workers list
│   └── /sessions/*              → shared session management
│
└── Worker (0.0.0.0:port, optional)
    ├── /worker/runs             → start agent run
    ├── /worker/runs/{id}/events → SSE event stream
    ├── /worker/runs/{id}/follow → follow-up turn
    ├── /worker/cancel           → cancel run
    ├── /worker/health           → health check
    └── /worker/capabilities     → worker capabilities
```

## Local Tools

| Tool | Description |
|------|-------------|
| `read_file` | Read UTF-8 text file (workspace-relative, offset/limit) |
| `write_file` | Create/overwrite UTF-8 file |
| `search_replace` | Exact text replacement in file |
| `grep` | Rust-regex search across workspace |
| `bash` | Non-interactive shell command (30s default, 120s max) |

## Key Decisions

- **Auth**: Worker API uses single `pairing_token`, no rotation, no rate limiting. Acceptable for personal use.
- **Discovery**: LAN subnet scan (`/24`) + Tailscale peers, probed every 20s. No throttling — personal use only.
- **Tool loop**: Capped at 8 iterations. Truncation emits visible message with option to continue.
- **LLM providers**: Config-driven via `llm.toml`. Supports OpenAI, Anthropic, Ollama, Google, Groq, XAI, Aliyun.
- **License**: `warpui_core`/`warpui` under MIT, rest under AGPL v3.

## Config Location

All config lives in `~/.warp-oss/`:
- `llm.toml` — LLM providers, models, system prompts, tool permissions
- `agent-worker.toml` — worker listener config
- `local-account.json` — local identity
- `settings.toml` — Warp client settings
- `keybindings.yaml` — custom keybindings
