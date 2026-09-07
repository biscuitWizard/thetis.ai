---
name = "mooR clients and the web UI contract"
brief = "What lives under clients/ — Meadow, the Flutter client, the web SDK, moor-web-mcp — and the contract moor-web-host offers them: HTTP, WebSocket frames, auth, and history resume."
when_to_use = "Use when building or debugging a mooR client: Meadow, meadow_flutter, @moor/web-sdk, moor-web-mcp, or a client of your own. Use it for the web host's HTTP surface and openapi.yaml, the WebSocket subprotocol and its frame markers, X-Moor auth headers, OAuth2 from a browser or a desktop app, reattach and history resume, presentations and content types, or the npm workspace and how a client is built and served next to the Rust binaries. Use it when a frame is not understood, a token expires mid-session, history will not replay, or a client and daemon are of different vintages. Not for the web host's own internals (read hosts-and-sessions), not for the RPC between processes (read daemon-and-rpc), not for the .fbs files (read wire-schema), not for the Torchship game database or in-world MOO verb authoring, and not for Thetis's own internals."
universal = false
tags = ["moor", "meadow", "web client", "web-sdk", "flutter", "websocket", "openapi", "http api", "oauth2", "presentations", "typescript", "npm workspace", "vite", "client"]
version = 1
---

# mooR clients and the web UI contract

A mooR client is any program that speaks the web host's HTTP and WebSocket
surface. Nothing about the world is client-side: a client renders events and
sends commands. This skill is the contract that surface offers, and the state of
the clients that consume it.

## What lives under clients/

| Directory | What it is | Built by |
|---|---|---|
| `clients/meadow` | The React and Vite web client, also packaged as a Tauri desktop app. The reference implementation | npm workspace |
| `clients/meadow_flutter` | A Flutter client for web and Linux desktop, with Android and iOS structurally possible | Flutter, not npm |
| `clients/web-sdk` | `@moor/web-sdk`: the shared TypeScript protocol layer. Auth headers, HTTP wrappers, WebSocket attach and dispatch, FlatBuffer decoding, narrative and presentation parsing | npm workspace |
| `clients/moor-web-mcp` | `@moor/web-mcp`: a stdio MCP server built on the web SDK. See `mcp-host` for how it compares with `moor-mcp-host` | npm workspace |
| `crates/schema/schema` | `@moor/schema`: the generated TypeScript FlatBuffer bindings. It is an npm workspace even though it lives under `crates/` | npm workspace |

The Flutter client's own README states it is replacing the React client;
`clients/README.md` still describes both as maintained on independent version
lines. Treat that as a transition in progress and check which one a deployment
actually serves before assuming.

The two clients share no code. The web SDK is TypeScript; the Flutter client
generates its own Dart bindings from the same `.fbs` files with
`clients/meadow_flutter/tool/gen_flatbuffers.sh`, which pins the same `flatc`
version CI installs. So the schema is shared and the protocol layer is not. A
fix in `@moor/web-sdk` does not reach the Flutter client.

## The HTTP surface

`crates/web-host/openapi.yaml` is the reference. It is embedded in the binary and
served at `/openapi.yaml`, and `book/src/web-client/http-api-reference.md` is
generated from it by `tools/generate-api-docs.py` — the page carries a banner
saying so. Read the spec, not a list copied into a skill.

The shape, which is stable:

- `/auth/connect` and `/auth/create` take a form and return tokens. `/auth/validate`
  and `/auth/logout` manage them.
- `/v1/...` covers world state: eval, objects, verbs, properties, presentations,
  history, features, and a batch endpoint that carries several world-state
  actions in one request.
- `/ws/attach/connect` and `/ws/attach/create` upgrade to a WebSocket.
- `/health`, `/version` and `/openapi.yaml` need no authentication.
- `/webhooks/*` and the OAuth2 routes exist only when enabled.

**Two response formats.** Most `/v1` endpoints negotiate on `Accept` between
`application/x-flatbuffers` (the default when the header is absent or `*/*`) and
`application/json`. Neither acceptable gives 406. Send an explicit `Accept`; do
not rely on the default staying what it is.

Nothing verifies that `openapi.yaml` matches the router. It is currently in sync
— every path in `crates/web-host/src/routes.rs` appears in the spec — but a route
added without a spec edit would not fail CI.

## Authentication

Three headers carry a session on HTTP requests:

| Header | Carries |
|---|---|
| `X-Moor-Auth-Token` | The PASETO `AuthToken` for the player |
| `X-Moor-Client-Token` | The PASETO `ClientToken` for the connection |
| `X-Moor-Client-Id` | The client UUID |

Only the auth token is required; the other two let the web host reuse an existing
connection instead of making a fresh one. Read `daemon-and-rpc` for what each
token proves.

**The WebSocket does not use headers.** A browser cannot set them on an upgrade,
so credentials travel in the `Sec-WebSocket-Protocol` list: `moor` first, then
`paseto.<authToken>`, then optionally `client_id.<uuid>` and
`client_token.<token>`, plus `initial_attach.true` on a first attach. The server
answers with the `moor` subprotocol alone. `buildWsAttach` in the web SDK is the
canonical construction; copy its ordering rather than inventing your own.

This is also why a token in a subprotocol string ends up in proxy logs. Terminate
TLS in front of the web host and be careful what the proxy records.

**OAuth2**, when enabled, has two flows. The browser flow is cookie-bound:
authorize, provider callback, then exchange a one-time code. The app flow is
proof-bound for desktop and mobile, and the web host will only redirect to a URI
matching a configured allowed prefix. Both end either with MOO tokens or with a
verified external identity plus an account-choice step for a new user. Neither
flow gives a client authority the resulting player does not have.

## The WebSocket frames

Deliberately minimal. There is no envelope and no JSON control channel.

**Client to server**

| Frame | Meaning |
|---|---|
| Binary, one byte `0x00` | Keepalive. Ignored |
| Binary, one byte `0x01` | Heartbeat response |
| Binary starting `0x03` | WebRTC signalling, a JSON envelope after the prefix byte |
| Any other text or binary | A command, or a reply to a pending input request |

**Server to client**

| Frame | Meaning |
|---|---|
| Binary, one byte `0x02` | Heartbeat request. Answer with `0x01` |
| Binary starting `0x03` | WebRTC signalling answer or ICE candidate |
| Any other binary | An encoded `ClientEvent` FlatBuffer |

The distinction between a command and an input reply is **positional**, not
tagged: whether the server is currently expecting input decides how it reads your
frame. A client must therefore track its own input-request state, and must not
send a command while an input request is outstanding.

**The first frame on every connection is a `CredentialsUpdated` event** carrying
the client id and client token, with sequence zero. Store both. They are what
makes a later reattach possible, and they are re-sent on every connection,
including one created after a reattach failed.

If WebRTC is configured, the client may offer an SDP offer over the WebSocket and
create a data channel; the server answers and then routes `DataEvent`s whose
domain is in the configured realtime set over that channel instead. It is an
optimisation, never a requirement. Everything still works on the WebSocket alone.

## Resuming after a reconnect

Two independent mechanisms, easy to confuse.

**Session resume.** Reconnect with the stored `client_id` and `client_token`
subprotocols and *without* `initial_attach.true`. The web host then tries a
reattach. If it succeeds the connect type is `Reconnected`; if it fails, or if
the credentials were stale, it falls back to a fresh attach rather than failing
the upgrade — so a client can always reconnect, but may silently become a new
connection. Watch the connect type; do not assume continuity. Sending
`initial_attach.true` suppresses the reattach attempt entirely, which is what a
fresh login should do.

**History resume.** Scrollback is a separate `GET /v1/history` call, with one of
`since_seconds`, `since_event` or `until_event`, plus a limit. It returns
**encrypted blobs**, which the client decrypts with the age identity it derived
from the player's event-log password. `GET /v1/presentations` behaves the same
way: it returns `{id, encrypted_blob}` pairs, so restoring open panels needs the
same key. A client with no key gets nothing back and must not present that as an
error.

Read `event-log-and-history` before implementing either. The rule that decides
whether history exists at all is server-side and has nothing to do with the
client.

## Presentations and structured output

A narrative event is one of five kinds: notify, present, unpresent, traceback,
and data. Only `notify` is text. The others are the structured surface:

- **Present** carries a `Presentation`: an id, a content type, content, a
  `target` string, and a bag of string attributes. Re-sending the same id
  replaces the existing presentation.
- **Unpresent** removes one by id.
- **Data** carries a domain, a kind and a `Var` payload. It is a non-visual
  state channel, and it is what the WebRTC realtime path is for.

**`target` is a client convention, not a server contract.** The daemon stores it
as an opaque string. Meadow defines the vocabulary — semantic targets like
`navigation`, `inventory`, `status`, `tools`, `communication`, `help`; explicit
docks `left`, `right`, `top`, `bottom`; `window`; and a set of editor and dialog
targets — and maps them differently on desktop and mobile. An unknown target
falls to the right dock, or the bottom on mobile. `book/src/web-client/presentations.md`
documents that vocabulary and matches the code. Another client may choose
differently, and MOO code that hard-codes Meadow's targets is coupled to Meadow.

**How a line-oriented client degrades.** The telnet host shows the intended
behaviour: it renders djot and markdown to a terminal, and it **ignores present,
unpresent and data events entirely**. It does not try to draw a panel in text.
Both content-type spellings are accepted, with and without a slash
(`text_djot` and `text/djot`), and anything unrecognised falls back to plain
text. The web SDK normalises identically. So the degradation rule across the
whole system is: **an unknown content type is plain text, and a structured event
a client cannot render is dropped, not approximated.**

## Stable, versus what you must detect

| Part of the contract | Status |
|---|---|
| The three `X-Moor-*` header names | Stable |
| The WebSocket subprotocol scheme and the one-byte frame markers | Stable |
| `CredentialsUpdated` as the first frame | Stable |
| Path shapes under `/auth`, `/v1`, `/ws/attach` | Stable; confirm against `/openapi.yaml` |
| The set of `ClientEvent` variants | **Grows.** Ignore what you do not know |
| The set of narrative event kinds and content types | **Grows.** Fall back, never fail |
| Presentation targets and attributes | **A convention.** Negotiate with the world's authors, not with the server |
| Server features: symbols, booleans, custom errors, rich notify, event log, anonymous objects and the rest | **Detect** with `GET /v1/features` |
| WebRTC, webhooks, OAuth2, CORS, rate limiting | **Optional.** Present only if the operator enabled them |
| The FlatBuffer schema itself | Evolves under the rules in `wire-schema` |

`/v1/features` is the feature-detection point. The web host caches the answer for
the life of the process because features do not change at runtime, so a client
may fetch it once at startup. Never infer a feature from a version string.

**Unknown events must be ignored, and this is live today, not hypothetical.** The
web SDK's dispatcher has an explicit unknown branch, and `TaskSuspendedEvent`
already exists in the schema union with no case in the SDK, so it lands there
now. A client that throws on an unrecognised variant will break on the next
daemon release.

## Building and serving

The repository root is one npm workspace covering `clients/meadow`,
`clients/web-sdk`, `clients/moor-web-mcp` and `crates/schema/schema`. Order
matters, and the root scripts encode it:

1. `npm ci` at the root. Always at the root, so `@moor/schema` and
   `@moor/web-sdk` resolve to this checkout.
2. `npm run web:prepare` builds the schema bindings, then the SDK. The schema
   step needs `flatc`; see `wire-schema`.
3. `npm run web:build` does that and then builds Meadow.
4. `npm run full:build` adds a release build of the single-process `moor` binary.

For development, `npm run full:dev` runs Meadow's Vite server and the
single-process server together. Vite proxies the API, WebSocket, auth, health,
version and webhook paths to the web host, so the browser sees one origin. The
Flutter client does the same with its own Vite proxy script. The port numbers
live in `clients/meadow/vite.config.ts`, `moor-dev.yaml` and the web host's
arguments; read them there rather than from memory.

In production Meadow is **static files**. It is not embedded in any Rust binary.
A frontend server — nginx in the shipped configuration — serves the build output
and reverse-proxies the same path prefixes to the web host. That is why CORS is
normally not needed and is off by default: a browser client is same-origin. Turn
CORS on only for a genuinely cross-origin client, such as a packaged desktop app.
For how that is deployed and released, read
`moor/working-in-the-repo/deployment-and-release`.

## Invariants

1. **A client renders and sends; it decides nothing.** Any client-side check is a
   convenience. The world enforces its own rules.
2. **An unknown event variant, content type or presentation target is ignored or
   degraded, never fatal.**
3. **The first WebSocket frame is `CredentialsUpdated`, and a client must store
   what it carries.** Without it there is no reattach.
4. **A frame is a command or an input reply according to session state.** Track
   the pending input request.
5. **History and presentation restore need the event-log key.** No key means an
   empty scrollback, which is a normal state and not an error.
6. **The schema is shared; the protocol layer is not.** A protocol fix must be
   made once per client stack.
7. **Build from the repository root.** A workspace package built in isolation
   resolves the wrong `@moor/schema`.

## Where the code lives

| Path | Responsibility |
|---|---|
| `crates/web-host/openapi.yaml` | The HTTP surface, embedded and served at `/openapi.yaml` |
| `crates/web-host/src/routes.rs` | The router, CORS, rate limits, body limits |
| `crates/web-host/src/host/web_host.rs` | Subprotocol parsing, the attach and reattach decision, features, health, version |
| `crates/web-host/src/host/session/mod.rs` | The WebSocket loop, the first credentials frame, realtime routing |
| `crates/web-host/src/host/session/websocket.rs` | Frame markers and how a client frame is classified |
| `crates/web-host/src/host/auth/` | Login, the ephemeral extractor, OAuth2 |
| `crates/web-host/src/host/handlers/` | The `/v1` endpoints, including history and presentations |
| `clients/web-sdk/src/ws.ts`, `ws-session.ts`, `ws-dispatch.ts` | Attach construction, control frames, event dispatch |
| `clients/web-sdk/src/auth.ts`, `history.ts`, `narrative.ts`, `presentations.ts` | Headers, encrypted history parsing, content-type normalisation |
| `clients/meadow/src/hooks/usePresentations.ts`, `src/types/presentation.ts` | The reference target vocabulary and its placement rules |
| `clients/meadow_flutter/tool/gen_flatbuffers.sh` | The Dart binding generation and its pinned `flatc` |
| `tools/generate-api-docs.py` | Regenerates the book's API reference from the spec |
| `book/src/web-client/` | Presentations, output, accessibility, OAuth2 and deployment, from the client author's side |

## Failure branches

| Symptom | Cause | Action |
|---|---|---|
| A frame the client does not understand | A newer daemon sent an event variant the client has no case for | Route it to an ignore branch and log once. Never throw. `TaskSuspendedEvent` already does this to the web SDK |
| The WebSocket upgrade returns 401 | The auth token is missing, malformed, or not in the subprotocol list | Check the protocol list order and that `paseto.` prefixes the token |
| The upgrade succeeds but the session is new every time | The client sent `initial_attach.true`, or the stored client credentials were stale | Send the stored `client_id` and `client_token` and omit the initial-attach marker. Read the connect type in the reply |
| A token expires mid-session | The player's session was invalidated, or the connection went stale | HTTP calls return 401. Re-authenticate and reattach; the credentials frame on the new connection replaces what you stored. Do not retry the same token |
| History returns nothing at all | The player has no event-log key, or the feature is off on the server | Both are normal. Distinguish them with `/v1/features` and the pubkey endpoint before showing an error |
| History returns blobs that will not decrypt | The password changed, or the key derivation differs from the reference client | Match the derivation in `doc/event-log-encryption-design.md`. Old records need the old key |
| Panels do not come back after reconnect | Presentation restore is encrypted too, and needs the same key | Unlock before calling `/v1/presentations` |
| A `/v1` call returns 406 | The `Accept` header asked for something neither format satisfies | Send `application/json` or `application/x-flatbuffers` |
| A `/v1` call returns binary when JSON was expected | No `Accept` header, so the FlatBuffers default applied | Always send `Accept` explicitly |
| Decoding fails on every event | The client was built against a different schema vintage than the daemon | Rebuild the client from the same checkout. There is no protocol version negotiation; read `wire-schema` |
| CORS errors in a browser | The client is being served cross-origin with CORS off | Serve it same-origin behind a proxy, which is the intended shape, or enable and configure CORS on the web host |
| A presentation appears in the wrong place | The world used a target this client does not know | Meadow falls back to the right dock. Agree the vocabulary with the world's authors |
| Editing the book's API reference has no effect | It is generated | Edit `crates/web-host/openapi.yaml` and rerun `tools/generate-api-docs.py` |

## Read first / read next

- Read `hosts-and-sessions` for the server side of everything here, including the
  handler object that decides which login policy a port uses.
- Read `event-log-and-history` before implementing scrollback or panel restore.
- Read `wire-schema` before regenerating bindings in any language.
- Read `moor/working-in-the-repo/deployment-and-release` for how the client is
  packaged and served alongside the Rust binaries.
- `book/src/web-client/` is written for client authors and its presentations page
  matches the code. Verify anything else there against `crates/web-host`.
