# WarpSOLO Bonjour Remote Agents

## Status

Active design. This is higher priority than the browser `/remote-control` work.

## Goal

WarpSOLO instances on the same LAN or Tailscale network should discover each other and allow one instance to run an agent on another machine. The user should not need a Warp account, Warp cloud agent service, or manual endpoint setup.

The first useful version should answer this question:

> "Which of my nearby machines should run this coding agent, and can I start it there from this WarpSOLO UI?"

## Current Local Agent Architecture

WarpSOLO already has a local sidecar in `app/src/bin/oss_loopback.rs`.

Relevant current behavior:

- `LoopbackServer::spawn()` starts an HTTP server bound to `127.0.0.1:0`.
- It stubs account and GraphQL operations needed by the OSS app.
- `~/.warp-oss/local-account.json` supplies a stable local user/device identity.
- `~/.warp-oss/llm.toml` defines local LLM models and provider headers.
- `/ai/multi-agent` accepts Warp's Multi-Agent API protobuf request and runs a local OpenAI-compatible tool loop.
- The tool loop already supports read, write, search-replace, grep, and shell.
- The existing ambient/cloud-agent UI and CLI code still expect `AIClient` methods like `spawn_agent`, `get_ambient_agent_task`, and follow-up submission.

This means the remote-agent feature should extend the sidecar into a local agent coordinator, not fork a separate product path.

## Proposed Architecture

Each WarpSOLO instance runs two logical services inside the sidecar:

1. **Coordinator API**
   - Runs on the local UI instance.
   - Owns discovery cache and task routing.
   - Exposes cloud-agent-compatible endpoints to the existing UI where practical.

2. **Worker API**
   - Runs on every participating WarpSOLO instance, including the local one.
   - Executes agent runs in a workspace on that machine.
   - Streams task status, tool events, terminal/session join info, and final output back to the coordinator.

Bonjour/mDNS advertises the Worker API.

```text
WarpSOLO A (coordinator)
  -> mDNS browse _warpsolo-agent._tcp.local
  -> discovers WarpSOLO B, C
  -> user selects host B
  -> coordinator POST /worker/runs on B
  -> B runs local agent loop
  -> coordinator subscribes to B run events
  -> existing Warp agent UI renders progress
```

## Bonjour Service

Service type:

```text
_warpsolo-agent._tcp.local
```

TXT records:

```text
version=1
device_id=local-device-<hostname>
user_id=local-user-<username>
hostname=<system-hostname>
display_name=<username>@<hostname>
app=WarpSOLO
api=http
capabilities=agent,terminal,workspace
auth=pairing-token-v1
```

The advertised port should be a LAN/Tailscale-bindable worker listener, not the existing `127.0.0.1` UI loopback port. Keep the existing loopback listener private. Add a separate listener only when remote agents are enabled.

## Security Model

Do not expose arbitrary unauthenticated code execution on the LAN.

V1 should use a simple local pairing token:

- Each worker has `~/.warp-oss/agent-worker.toml`.
- Config contains `enabled`, `bind`, `port`, and `pairing_token`.
- Bonjour advertises only if `enabled = true`.
- Coordinator calls worker APIs with `Authorization: Bearer <pairing_token>`.
- The UI can start with manual token entry; automatic trust can come later.

Example:

```toml
enabled = true
bind = "0.0.0.0"
port = 0
pairing_token = "generated-long-random-token"
```

Later, replace or augment this with local approval and device keys.

## Worker API V1

Use HTTP plus SSE first. It is easier to test and maps cleanly onto the current agent event model.

Endpoints:

```text
GET  /worker/health
GET  /worker/capabilities
POST /worker/runs
GET  /worker/runs/{run_id}
GET  /worker/runs/{run_id}/events
POST /worker/runs/{run_id}/followup
POST /worker/runs/{run_id}/cancel
```

`POST /worker/runs` request:

```json
{
  "prompt": "fix the failing tests",
  "workspace": "/Users/tung/Projects/foo",
  "model_id": "optional-model-id",
  "harness": "local-openai",
  "source_device_id": "local-device-macbook"
}
```

Event stream:

```json
{ "type": "state", "state": "pending" }
{ "type": "tool_call", "name": "grep", "id": "..." }
{ "type": "tool_result", "id": "...", "summary": "..." }
{ "type": "output_delta", "text": "..." }
{ "type": "session_started", "session_id": "...", "session_link": "warposs://shared_session/..." }
{ "type": "finished", "state": "succeeded" }
```

The worker should persist enough run state in memory for V1. Disk persistence can wait.

## UI Integration

Reuse the existing host selector mental model:

- Add discovered hosts as `Host::SelfHosted { slug }` choices.
- For WarpSOLO, `worker_host` should mean a discovered local worker ID, not a cloud-side self-hosted worker slug.
- Selecting a remote host should route the ambient-agent spawn request to the coordinator path.

Implementation path:

1. Create an `OssAgentCoordinator` or equivalent behind the OSS `AIClient` path.
2. For `worker_host = None | "warp"`, run on the local sidecar worker.
3. For `worker_host = <device_id>`, look up the discovered worker and call its Worker API.
4. Return `AmbientAgentTask`-compatible status objects so the existing ambient-agent UI does not need a new rendering stack.

## Workspace Selection

V1 should be explicit. A remote worker cannot assume the same path exists.

Options:

- User enters a workspace path on the target host.
- Coordinator remembers per-host recent workspaces.
- Later: discover git repos on each worker and expose a picker.

Do not silently map local paths to remote paths in V1.

## Implementation Plan

1. Add config file support for `~/.warp-oss/agent-worker.toml`.
2. Add a second listener in `oss_loopback` for LAN worker APIs when enabled.
3. Add mDNS publish for `_warpsolo-agent._tcp.local`.
4. Add mDNS browse and a discovery cache in the coordinator.
5. Add `GET /worker/health` and `GET /worker/capabilities`.
6. Add `POST /worker/runs` using the existing local OpenAI-compatible agent loop.
7. Add `GET /worker/runs/{run_id}/events` as SSE.
8. Add local task registry and `AmbientAgentTask` conversion.
9. Wire OSS `AIClient::spawn_agent` to choose local or discovered worker based on `worker_host`.
10. Surface discovered workers in the existing host selector.
11. Add manual tests with two machines over Tailscale.

## Rust Crate Options

Preferred discovery crate should be small and maintained. Candidates to evaluate:

- `mdns-sd`: pure Rust service daemon style API, good fit for cross-platform mDNS publish/browse.
- `libmdns`: simple publish path, but browse support and maintenance need checking.
- Native Bonjour via `dns-sd`/`NSNetService` on macOS only is possible, but WarpSOLO should eventually work outside macOS.

Choose after a quick spike that proves publish + browse on macOS and Tailscale.

## Open Questions

- Should remote workers run agents in the foreground terminal/session, or spawn a managed PTY with session sharing?
- Should the coordinator proxy all worker events, or should the UI connect directly to the worker after spawn?
- How much of the cloud-agent `AIClient` model can be reused before it becomes harder than a small OSS-only path?
- Should a worker expose its configured `llm.toml` models, or should the coordinator send model/provider config with each run?

## Near-Term Recommendation

Implement discovery and worker execution before polishing `/remote-control`.

The remote-agent feature gives immediate daily value: start a coding agent on whichever machine has the repo, hardware, credentials, or network access. The browser remote-control layer can then reuse the same pairing, discovery, and worker listener infrastructure.
