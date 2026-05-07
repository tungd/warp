# WarpSOLO Remote Control: Pending Design

## Status

Pending.

The current WarpSOLO build has a local in-process session-sharing broker, but it does not yet have a browser renderer, public tunnel, or phone-oriented controller page. The broker is useful groundwork because it lets WarpSOLO create, join, resume, and relay shared-session protocol messages without Warp cloud.

## Current Implementation

Commit `a3f83a2` added a local loopback session-sharing broker in `app/src/bin/oss_loopback.rs`.

Implemented:

- `GET /sessions/create` websocket for sharer session creation.
- `GET /sessions/join/{session_id}` websocket for viewers.
- `GET /sessions/{session_id}/resume` websocket for sharer reconnect.
- In-memory session state with scrollback, active prompt, window size, ordered terminal events, participant list, presence, roles, and reconnect token.
- Viewer joins are read-only by default.
- Only one viewer can hold an execution role at a time.
- Ordered terminal events are stored and replayed to reconnecting viewers.
- `ChannelState` supports a separate `session_sharing_public_root_url`, so copied links do not have to use Warp's cloud app root.
- WarpSOLO points both the private websocket endpoint and public share root to the local loopback server.
- `GET /session/{session_id}` returns a minimal local HTML page that redirects back to `warposs://shared_session/{session_id}`.

This is enough for native-to-native local shared-session flows. It is not yet the intended web remote-control feature.

## Target Behavior

The intended feature is single-user remote control:

- User starts sharing a WarpSOLO terminal.
- WarpSOLO creates a stream ID.
- WarpSOLO serves a web page that renders the terminal.
- The page is read-only by default.
- The page can request "take control".
- At most one controller is active.
- A phone should be able to connect and send input.
- Late-join replay is not required for the first version.
- Public access can be provided by launching `cloudflared tunnel --url <local-url>` and showing/copying the generated URL.

## Proposed Architecture

Keep the existing session-sharing broker as the source of truth for Warp protocol state, but add a separate web bridge:

```text
WarpSOLO terminal
  -> session_sharing_protocol sharer websocket
  -> oss_loopback session broker
  -> web bridge websocket/SSE
  -> xterm.js or wterm browser renderer
```

The web bridge should not make the browser speak Warp's private session-sharing protocol directly. It should expose a narrow browser protocol:

- `snapshot`: terminal dimensions and current terminal state if available.
- `pty_bytes`: bytes read from the sharer.
- `resize`: terminal dimensions.
- `presence`: read-only/controller state.
- `control_granted` / `control_denied`.
- `input`: browser-to-host bytes, accepted only for the active controller.

For V1, the web bridge can subscribe to the same `OrderedTerminalEvent` stream that native viewers receive. For terminal rendering, prefer xterm.js first because it is mature and handles PTY bytes directly.

## Open Design Points

- **Renderer**: xterm.js is the lowest-risk V1. wterm is still worth evaluating, but only after the HTTP/websocket shape is stable.
- **Scrollback**: V1 can be live-only. Existing scrollback replay can be added once the web bridge knows how to translate `ScrollbackBlock` into terminal-visible content.
- **Auth**: V1 can use an unguessable stream token in the URL. A local confirmation gate can be added before granting control.
- **Cloudflared lifecycle**: WarpSOLO should spawn it as a child process, parse the generated URL, and kill it when sharing stops.
- **Clipboard**: Out of scope for V1. Add after keyboard/input works.

## Next Steps

1. Add a web bridge state object beside the local session broker.
2. Add `POST /remote-control/create/{session_id}` to allocate a stream token.
3. Add `GET /remote-control/{stream_token}` to serve the browser page.
4. Add `GET /remote-control/{stream_token}/ws` for browser websocket events.
5. Implement live PTY byte fanout to xterm.js.
6. Implement read-only default mode and `take_control` role request.
7. Spawn and supervise `cloudflared` for public links.
8. Add a manual validation flow: local browser first, then phone over LAN, then phone over Cloudflare tunnel.

## Non-Goals For Now

- Multi-user collaboration.
- Persistent shared-session archive.
- Rich Warp block UI in the browser.
- Full native Warp session-sharing ACL model.
- Cloud-hosted rendezvous.
