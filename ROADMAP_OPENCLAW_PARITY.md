# IronClaw → OpenClaw Feature Parity Roadmap

> **Active development base:** `dev-local`  
> **Last updated:** 2026-03-08  
> **Version at time of writing:** 0.10.0  

This document is the single source of truth for bringing `mycosavant/ironclaw-rs` up to feature parity with `openclaw/openclaw` **without compromising IronClaw's security model**. It is designed to be actionable: every item has a role-based owner, measurable acceptance criteria, explicit security notes, and a module pointer.

---

## Table of Contents

1. [IronClaw Current Capabilities](#1-ironclaw-current-capabilities)
2. [OpenClaw Product Areas and Gap Map](#2-openclaw-product-areas-and-gap-map)
3. [Feature Comparison Matrix](#3-feature-comparison-matrix)
4. [Phased Milestones](#4-phased-milestones)
   - [Phase 1 – 0–3 Months (Foundation & High-Value Gaps)](#phase-1--03-months)
   - [Phase 2 – 3–6 Months (Ecosystem Depth)](#phase-2--36-months)
   - [Phase 3 – 6–12 Months (Platform Completeness)](#phase-3--612-months)
5. [Security-Preserving Design Approach](#5-security-preserving-design-approach)
6. [Implementation Strategy](#6-implementation-strategy)
7. [Testing and Release Plan](#7-testing-and-release-plan)
8. [Risk Register](#8-risk-register)
9. [Upgrade and Compatibility Plan](#9-upgrade-and-compatibility-plan)

---

## 1. IronClaw Current Capabilities

IronClaw is a **security-first personal AI assistant** written in Rust. The following capabilities are already production-ready at v0.10.0:

### Core Runtime
- **Multi-channel input**: TUI (Ratatui), HTTP webhook, REPL, WASM channels (Telegram, Slack, Discord, Signal), WebChat
- **Parallel job execution** with state-machine scheduler and self-repair for stuck jobs
- **Session management**: per-sender sessions, context compaction, undo/redo with checkpoints
- **Agent loop**: multi-turn reasoning with tool approval overlay and cycle detection (SHA-256 sliding window)

### LLM Providers
- **NEAR AI** (primary, with session-token auto-renewal)
- **OpenAI-compatible** (OpenRouter, Together AI, Fireworks, vLLM, Ollama)
- **Tinfoil** (private inference, IronClaw-only)
- **Multi-provider failover chain** with exponential backoff and cooldown management
- **Smart routing**: cost-optimised model cascade for simple tasks
- **Block-level and tool-level streaming** via SSE

### Tools and Extensibility
- **Built-in tools**: file, shell, HTTP, JSON, time, memory, browser (Playwright), job, routine, workflow, extension, skill
- **WASM tool sandbox** (wasmtime, capability-based, fuel-metered, memory-limited)
- **MCP client** (JSON-RPC over HTTP)
- **Dynamic tool builder** (scaffolding, testing, WASM validation)
- **WASM channels**: hot-activatable extensions for messaging platforms

### Memory and Knowledge
- **Persistent workspace** with hybrid search (BM25 full-text + pgvector embeddings, RRF fusion)
- **Embeddings batching**, flexible dimension support, Ollama embeddings backend
- **Memory hygiene** wired into heartbeat loop

### Security (IronClaw-differentiated)
- **AES-256-GCM secrets encryption** at rest
- **WASM sandbox** with allowlisted network endpoints and credential injector
- **Prompt injection defense**: pattern detection, sanitization, injection circuit breaker
- **Leak detector**: secret exfiltration prevention on outbound content
- **Ed25519 manifest signing** for skills and WASM tools
- **Merkle hash-chain audit trail** (tamper-evident job action log)
- **SHA-256 cycle guard** against repeating tool-call patterns
- **SSRF protection**: IPv4 + IPv6 transition bypass blocking in HTTP tool and webhooks
- **RBAC** on web gateway (Owner/Admin/User/Viewer)
- **TLS 1.3** via rustls throughout

### Infrastructure
- **Dual database backend**: PostgreSQL 15+ with pgvector, or embedded libSQL/Turso (AES-256-CBC encryption)
- **Heartbeat system**: proactive periodic execution with checklist
- **Workflow engine**: sequential, parallel, conditional, and loop steps with compiler validation
- **Routines**: cron, event trigger, webhook trigger; bundled hooks; outbound webhooks
- **Inter-agent messaging bus** (`agent_send` / `agent_spawn`)
- **Web gateway**: 40+ REST/SSE/WebSocket endpoints; RBAC; log streaming; OpenAI-compatible `/v1/chat/completions`
- **Setup wizard**: 7-step interactive onboarding
- **cargo-dist** releases: macOS (arm64/x86_64), Linux (arm64/x86_64), Windows (x86_64)

---

## 2. OpenClaw Product Areas and Gap Map

The table below maps each major OpenClaw product area to IronClaw's current state and the gap to close.

| OpenClaw Area | IronClaw Equivalent | Gap Summary |
|---|---|---|
| Multi-platform clients (macOS app, iOS app) | CLI + web gateway | 🚫 Out of scope (intentional) |
| Messaging channels | WASM channels (Telegram, Slack, Discord, Signal, WebChat) | WhatsApp, iMessage, Linq, LINE, Matrix, Mattermost, Google Chat, MS Teams, Voice, Nostr missing |
| Channel management UX | `ironclaw channels` CLI; web gateway channel dashboard | Streaming draft replies (Slack), per-group tool policies, ackReaction config, group session priming |
| Multi-agent routing | `agent/messaging.rs` (bus), `agent/task.rs` | No multi-agent workspace isolation; no `agents`/`sessions` CLI commands; no `--agent` flag |
| Automation (cron, hooks, webhooks) | Full routines + hooks system | Cron stagger, finished-run webhooks, `llm_input`/`llm_output` hooks, `transcribeAudio` hook missing |
| Extension ecosystem | WASM tools + skills + MCP | Plugin registry, browser `extraArgs`, Docker init scripts, Chromium-in-container |
| Web UI / Canvas | Web gateway with chat, logs, memory | Canvas/A2UI, config editing, agent management UI, i18n, dark-mode theme sync |
| Model provider breadth | NEAR AI, OpenAI-compat, Ollama, Tinfoil | AWS Bedrock, Google Gemini, NVIDIA API, Perplexity, MiniMax, GLM-5 missing |
| Media handling | image, audio, PDF, vision | Video support, TTS (Edge TTS), media caching missing |
| Gateway features | Web gateway with auth, RBAC, SSE/WS | Gateway lock (PID), launchd/systemd, mDNS/Bonjour, Tailscale, APNs, trusted-proxy auth, Presence |
| Security depth | Comprehensive (WASM sandbox, Ed25519, RBAC, leak detector, SSRF) | Device pairing, Tailscale identity, safe-bins allowlist, LD*/DYLD* validation, per-group tool policies |
| Development tooling | Rust/Cargo, clippy, rustfmt, GitHub Actions | Pre-commit hooks (prek-equivalent), Docker Chromium container support |

---

## 3. Feature Comparison Matrix

> Full per-feature status lives in `FEATURE_PARITY.md`. This matrix summarises by area for roadmap planning.

| Feature Area | OpenClaw | IronClaw | Status | Priority |
|---|---|---|---|---|
| **Architecture** | Hub-and-spoke | Hub-and-spoke | ✅ Parity | — |
| Multi-agent workspace isolation | ✅ | ❌ | Gap | P2 |
| **Gateway** | Full suite | 40+ endpoints | ✅ Core parity | — |
| launchd/systemd integration | ✅ | ❌ | Gap | P2 |
| mDNS/Bonjour discovery | ✅ | ❌ | Gap | P3 |
| Tailscale integration | ✅ | ❌ | Gap | P3 |
| Trusted-proxy auth | ✅ | ❌ | Gap | P2 |
| Presence system | ✅ | ❌ | Gap | P3 |
| APNs push pipeline | ✅ | ❌ | Gap | P3 |
| **Messaging – core** | Many channels | Telegram/Slack/Discord/Signal | ✅ Core parity | — |
| WhatsApp | ✅ | ❌ | Gap | P3 |
| iMessage / Linq | ✅ | ❌ | Gap | P3 |
| Matrix | ✅ | ❌ | Gap | P3 |
| LINE / Feishu / Mattermost | ✅ | ❌ | Gap | P3 |
| Google Chat / MS Teams / Twitch | ✅ | ❌ | Gap | P3 |
| Voice / Nostr | ✅ | ❌ | Gap | P3 |
| Telegram forum topics / channel_post | ✅ | ❌ | Gap | P3 |
| Slack streaming draft replies | ✅ | ❌ | Gap | P2 |
| **Agent system** | Full | Full (+ IronClaw extras) | ✅ Core parity | — |
| Global sessions | ✅ | ❌ | Gap | P3 |
| Elevated mode | ✅ | ❌ | Gap | P3 |
| Auth profiles / API key rotation | ✅ | ❌ | Gap | P3 |
| `agents`/`sessions` CLI | ✅ | ❌ | Gap | P2 |
| Z.AI tool_stream | ✅ | ❌ | Gap | P3 |
| llms.txt discovery | ✅ | ❌ | Gap | P3 |
| **Model providers** | Broad | NEAR AI + OpenAI-compat + Ollama + Tinfoil | 🚧 Partial | — |
| AWS Bedrock | ✅ | ❌ | Gap | P3 |
| Google Gemini | ✅ | ❌ | Gap | P3 |
| NVIDIA API | ✅ | ❌ | Gap | P3 |
| **Media** | Full | image/audio/PDF/vision | 🚧 Partial | — |
| Video support | ✅ | ❌ | Gap | P3 |
| TTS (Edge TTS) | ✅ | ❌ | Gap | P3 |
| Media caching | ✅ | ❌ | Gap | P3 |
| **Automation** | Full | routines + hooks + heartbeat | ✅ Core parity | — |
| Cron stagger controls | ✅ | ❌ | Gap | P3 |
| `llm_input`/`llm_output` hooks | ✅ | ❌ | Gap | P2 |
| `transcribeAudio` hook | ✅ | ❌ | Gap | P3 |
| Gmail pub/sub | ✅ | ❌ | Gap | P3 |
| **Security** | Comprehensive | Comprehensive + extras | ✅ Exceeds in some areas | — |
| Device pairing | ✅ | ❌ | Gap | P2 |
| Tailscale identity | ✅ | ❌ | Gap | P3 |
| Safe-bins allowlist | ✅ | ❌ | Gap | P2 |
| LD*/DYLD* env validation | ✅ | ❌ | Gap | P2 |
| Per-group tool policies | ✅ | ❌ | Gap | P2 |
| **Web UI** | Full | Core dashboard | 🚧 Partial | — |
| Canvas / A2UI | ✅ | ❌ | Gap | P3 |
| Config editing in UI | ✅ | ❌ | Gap | P3 |
| i18n (EN/ZH/PT) | ✅ | ❌ | Gap | P3 |
| Agent management UI | ✅ | ❌ | Gap | P3 |
| **Dev tooling** | TypeScript/pnpm/prek | Rust/cargo/clippy | ✅ Equivalent | — |
| Pre-commit hooks | ✅ | ❌ | Gap | P2 |
| Docker Chromium+Xvfb image | ✅ | ❌ | Gap | P3 |

---

## 4. Phased Milestones

### Milestone Conventions

- **Owner role**: A functional role (e.g., _Security Engineer_, _Platform Engineer_, _Channel Engineer_), not a named individual.
- **DoD (Definition of Done)**: The specific, verifiable outcome that closes the milestone item.
- **Dep**: Blocking dependency milestone IDs.
- Security notes appear inline under each item.

---

### Phase 1 – 0–3 Months

**Theme**: Close high-value security and platform gaps; harden existing features; unblock Phase 2 work.

#### 1.1 Security Hardening
_Owner: Security Engineer_

| Item | Module(s) | DoD | Security Note |
|---|---|---|---|
| **Safe-bins allowlist** for shell tool | `src/tools/builtin/shell.rs`, `src/safety/` | Configurable `SAFE_BINS_ALLOWLIST` env; default to hardened set; clippy clean | Prevents tool misuse via unexpected binary execution paths |
| **LD\*/DYLD\* env variable validation** | `src/tools/builtin/shell.rs` | Strip/reject `LD_PRELOAD`, `LD_LIBRARY_PATH`, `DYLD_INSERT_LIBRARIES` before subprocess spawn | Blocks dynamic linker injection attacks |
| **Per-group tool policies** | `src/agent/`, `src/channels/wasm/`, DB schema | Policy table keyed by `(group_id, tool_name)`; enforced in `ToolRegistry::resolve()`; UI toggle on web gateway | Limits blast radius if a channel is compromised |
| **Complete sandbox env sanitization** | `src/sandbox/container.rs`, `src/tools/builtin/shell.rs` | All secret-pattern env vars stripped from Docker container env _and_ subprocess env; integration test | Prevents credential leakage into untrusted containers |
| **Device pairing** | `src/channels/web/auth.rs`, new `src/pairing/` | Short-lived challenge/response codes; device registration in DB; revocation endpoint; DoS budget | Ensures only enrolled devices can use the gateway |
| **Trusted-proxy auth mode** | `src/channels/web/auth.rs` | `TRUSTED_PROXY_HEADER` env; validates header only when `TRUST_PROXY=true`; rejects if not configured | Enables reverse-proxy setups without exposing bearer token to proxy logs |

#### 1.2 Gateway Serviceability
_Owner: Platform Engineer_

| Item | Module(s) | DoD | Security Note |
|---|---|---|---|
| **launchd/systemd service files** | `deploy/` | Working `.plist` (macOS) and `.service` (Linux) with `ironclaw install-service`/`remove-service` subcommands | Run as non-root user; restrict file access via `ProtectSystem=strict` (systemd) |
| **Gateway PID lock** | `src/channels/web/mod.rs` | Lock file at `~/.ironclaw/gateway.pid`; `start` exits cleanly if already running; `stop` sends SIGTERM | Prevents duplicate gateway instances that could bypass auth state |
| **`agents` and `sessions` CLI commands** | `src/main.rs`, `src/agent/session_manager.rs` | `ironclaw agents list` and `ironclaw sessions list --agent <id>` output matching `ironclaw status` format | Output must not leak session tokens or credentials |
| **`llm_input` / `llm_output` hooks** | `src/agent/worker.rs`, `src/agent/routine.rs` | Hook points fire before LLM send and after LLM receive; documented in `FEATURE_PARITY.md` | Hook output sanitized through existing leak detector before injection into context |

#### 1.3 Channel Enhancements
_Owner: Channel Engineer_

| Item | Module(s) | DoD | Security Note |
|---|---|---|---|
| **Slack streaming draft replies** | `channels-src/slack/` | `on_status` callback updates draft; configurable `stream_mode`; unit tested | Draft content passes through existing prompt-injection sanitizer |
| **Self-message bypass** | `src/channels/wasm/wrapper.rs` | `sender_id == bot_id` check added to inbound filter; opt-in env flag | Prevents accidental routing loops |
| **Per-channel ackReaction config** | `channels-src/`, DB schema | `ack_reaction` field in channel config; default `✅`; hot-reloaded | No security impact; config validated to be a single Unicode grapheme cluster |

#### 1.4 Memory and Search
_Owner: Platform Engineer_

| Item | Module(s) | DoD | Security Note |
|---|---|---|---|
| **Temporal decay scoring** | `src/workspace/search.rs` | Recency weight in RRF; configurable half-life; benchmark test showing relevance improvement | Decay factor cannot be weaponised to suppress security-relevant memories |
| **Query expansion** | `src/workspace/search.rs` | Synonym expansion via LLM rewrite; toggle off by default | Expanded queries pass through same input validation as original |

---

### Phase 2 – 3–6 Months

**Theme**: Ecosystem depth; additional channels; multi-agent; web UI improvements.

#### 2.1 Multi-Agent Workspace Isolation
_Owner: Platform Engineer + Security Engineer_

| Item | Module(s) | DoD | Dep | Security Note |
|---|---|---|---|---|
| **Per-agent workspace namespacing** | `src/workspace/`, `src/agent/agent_loop.rs` | Each agent gets isolated DB prefix; `agent_id` scoping on all memory queries | — | Prevents cross-agent data leakage |
| **Multi-agent routing** | `src/agent/router.rs`, `src/agent/messaging.rs` | `MessageIntent::RouteToAgent` dispatches to named agent; load-balanced across running agents | workspace isolation | Agent-to-agent messages sanitized through existing injection defense |
| **`/subagents spawn` command** | `src/agent/submission.rs` | `/subagents spawn <skill>` launches task-scoped subagent; result returned to parent session | routing | Spawned agent inherits reduced tool policy from parent (least-privilege) |

#### 2.2 Additional Messaging Channels
_Owner: Channel Engineer_

| Item | Module(s) | DoD | Dep | Security Note |
|---|---|---|---|---|
| **WhatsApp** | `channels-src/whatsapp/` (new WASM) | Send/receive DMs; echo detection; pairing codes; integration test against WhatsApp Web sandbox | Phase 1 channel work | Same pairing and tool-policy framework as other WASM channels |
| **Matrix** | `channels-src/matrix/` (new WASM) | E2EE room membership; send/receive; pairing; integration test | — | E2EE keys stored via existing `secrets/crypto.rs` AES-256-GCM store |
| **Mattermost** | `channels-src/mattermost/` | Basic send/receive; emoji reactions; pairing | — | Outbound content filtered by leak detector |

#### 2.3 Gateway Improvements
_Owner: Platform Engineer_

| Item | Module(s) | DoD | Dep | Security Note |
|---|---|---|---|---|
| **Presence system** | `src/channels/web/server.rs`, SSE events | `agent.presence` SSE event; system-presence for background agents; beacon on connect | — | Presence data must not expose internal IP/PID to unauthorized clients |
| **mDNS/Bonjour discovery** | `src/channels/web/mod.rs` | Optional `mdns-sd` crate; `_ironclaw._tcp` service advertising; toggle via `GATEWAY_MDNS=true` | PID lock (1.2) | LAN-only; mDNS disabled on loopback-only bind |
| **Config editing via web UI** | `src/channels/web/server.rs`, static UI | `PATCH /api/config` endpoint; JSON schema validation; hot-reload triggered; RBAC Owner only | — | Config changes require Owner role; secret fields write-only (never returned in GET) |

#### 2.4 Model Provider Breadth
_Owner: LLM Engineer_

| Item | Module(s) | DoD | Dep | Security Note |
|---|---|---|---|---|
| **Google Gemini** | `src/llm/` (new `gemini.rs`) | `GEMINI_API_KEY` env; implements `LlmProvider` trait; failover-compatible; clippy clean | — | API key stored via `secrecy` crate; never logged |
| **AWS Bedrock** | `src/llm/` (new `bedrock.rs`) | IAM credential support via `aws-sdk-bedrockruntime`; streaming; clippy clean | — | STS credentials scoped to Bedrock Invoke only; no wildcard IAM |
| **Model auto-discovery** | `src/llm/mod.rs` | `GET /api/models` returns live model list from connected providers; cached with 5-min TTL | — | Provider API key not exposed in response |
| **Perplexity** | `src/llm/` (new `perplexity.rs`) | `PERPLEXITY_API_KEY`; web search with freshness param; implements `LlmProvider` | — | Search queries sanitized before sending to external API |

#### 2.5 Web UI Depth
_Owner: Frontend Engineer_

| Item | Module(s) | DoD | Dep | Security Note |
|---|---|---|---|---|
| **Agent management UI** | `src/channels/web/static/` | List agents, view sessions, kill agent from dashboard; RBAC Admin+ | multi-agent (2.1) | Actions require Admin role; displays masked session IDs only |
| **WebChat dark-mode theme sync** | `src/channels/web/static/` | `prefers-color-scheme` media query + toggle; persisted in localStorage | — | No security impact |
| **i18n foundation** (EN/ZH/PT) | `src/channels/web/static/` | `i18n.js` module; English + Simplified Chinese + Brazilian Portuguese; language selector | — | Translation strings sanitized against XSS before DOM insertion |

---

### Phase 3 – 6–12 Months

**Theme**: Platform completeness; P3 channels; canvas; advanced automation.

#### 3.1 Remaining P3 Channels
_Owner: Channel Engineer_

| Item | Notes |
|---|---|
| iMessage / Linq | Requires BlueBubbles or Linq API key; WASM channel |
| LINE | LINE Messaging API; WASM channel |
| Feishu/Lark | Bitable create app/field tools |
| Google Chat | Google Workspace bot |
| MS Teams | Outlook/Teams bot framework |
| Nostr | NIP-01/NIP-04 relay; E2EE via existing crypto primitives |
| Voice / Twilio | Twilio/Telnyx; TTS via Edge TTS; stale call reaper |
| Twitch | EventSub webhooks |

**Security note**: Each new WASM channel must pass the existing `crypto/signing.rs` Ed25519 manifest check before activation. Channel-specific pairing codes required before any tool access.

#### 3.2 Canvas / A2UI System
_Owner: Frontend Engineer + Platform Engineer_

| Item | Module(s) | DoD |
|---|---|---|
| **Canvas hosting** | `src/channels/web/server.rs` static + new `canvas/` module | Agent emits structured `canvas_update` SSE events; web UI renders agent-driven panels; sandboxed iframe rendering |
| **Canvas placement/resize** | static UI | Drag-resize panels; position persisted per session |

**Security note**: Canvas content rendered in sandboxed `<iframe sandbox="allow-scripts allow-same-origin">`; agent-provided HTML stripped of `<script>` before injection; CSP header enforced.

#### 3.3 Advanced Automation
_Owner: Platform Engineer_

| Item | Notes |
|---|---|
| Cron stagger controls | `CRON_DEFAULT_STAGGER_SECONDS`; jitter on concurrent jobs |
| Finished-run webhooks | Fire HTTP callback on job completion with HMAC-SHA256 signature |
| `transcribeAudio` hook | Pre/post-transcription hook point in media processing pipeline |
| Gmail pub/sub integration | Google Cloud Pub/Sub push subscription for Gmail events |
| Tailscale integration | `tailscale` API for peer auth; Tailscale IP binding for gateway |

#### 3.4 Remaining Security Gaps
_Owner: Security Engineer_

| Item | Notes |
|---|---|
| Tailscale identity | Peer certificate verification via Tailscale control plane API |
| Podman support | Drop-in Docker alternative; rootless container mode |
| Media URL validation | SSRF-safe URL check before fetching media in channel handlers |
| Dangerous tool re-enable warning | Warn when `gateway.tools.allow` re-enables HTTP tools that were removed from default set |
| Global session opt-in | Shared context sessions with explicit user consent; encryption at rest |

#### 3.5 Developer Experience
_Owner: Platform Engineer_

| Item | Notes |
|---|---|
| Pre-commit hooks (prek-equivalent) | `cargo fmt --check`, `cargo clippy`, secret scan on staged files |
| Docker Chromium + Xvfb image | Browser automation in container without X11 forwarding |
| Docker init scripts (`/ironclaw-init.d/`) | Extensible container init hooks for custom startup logic |
| Shell completion | `ironclaw completion bash/zsh/fish/powershell` |
| Self-update (`ironclaw update`) | cargo-dist updater integration |

---

## 5. Security-Preserving Design Approach

Every work item in this roadmap must be evaluated against the threat model below before implementation. These are non-negotiable constraints, not optional guidelines.

### 5.1 Threat Model Summary

| Threat | Mitigation Strategy |
|---|---|
| **Prompt injection** (malicious content in tool outputs or channel messages) | Existing `safety/sanitizer.rs` + injection circuit breaker; all new inbound paths must route through `Sanitizer::sanitize()` |
| **Secret exfiltration** via LLM responses | `safety/leak_detector.rs`; all outbound LLM content and channel messages checked before delivery |
| **SSRF** (server-side request forgery via HTTP/webhook tools) | `sandbox/proxy/allowlist.rs` + `tools/builtin/http.rs` SSRF guard; new HTTP-making code must use `DomainAllowlist::check()` |
| **WASM escape** (malicious tool overriding host memory) | `tools/wasm/limits.rs` fuel metering + memory cap; no new host functions without `security/` review |
| **Credential theft via env injection** | `tools/builtin/shell.rs` env scrubbing; Phase 1 adds LD*/DYLD* check; new subprocess paths must call `scrub_env()` |
| **Sandbox network breakout** | `sandbox/proxy/` allowlist enforced at TCP level; Docker `--network none` + proxy injection |
| **Tool policy bypass** | Per-group tool policies (Phase 1.1); policy checked in `ToolRegistry::resolve()`, not in individual tools |
| **Audit log tampering** | Merkle hash-chain (`history/chain.rs`); chain verification in `ironclaw doctor` |
| **Manifest forgery** (malicious WASM/skill install) | Ed25519 verification (`crypto/signing.rs`); trust store at `~/.ironclaw/trusted_keys/*.hex` |
| **Replay attacks on pairing codes** | Short-lived (5-min) challenge/response codes; nonce stored in DB with TTL |
| **XSS via agent-generated content** | Canvas content in sandboxed iframe; i18n strings DOM-text-only (no innerHTML) |

### 5.2 Sandbox Boundaries

```
┌─────────────────────────────────────────────────┐
│  IronClaw Process (Rust, non-root)               │
│  ┌────────────┐   ┌──────────────────────────┐  │
│  │ Agent Loop │──▶│  ToolRegistry             │  │
│  └────────────┘   │  ┌──────────┐  ┌───────┐ │  │
│                   │  │ Built-in │  │  WASM  │ │  │
│                   │  │  Tools   │  │  Sand  │ │  │
│                   │  └──────────┘  │  box   │ │  │
│                   │               │ (fuel  │ │  │
│                   │               │  meter)│ │  │
│                   │               └───────┘ │  │
│                   └──────────────────────────┘  │
│  ┌────────────────────────────────────────────┐ │
│  │ Docker Sandbox (orchestrator → worker)     │ │
│  │  ┌──────────┐  ┌────────────────────────┐ │ │
│  │  │ Container │  │ Network Proxy          │ │ │
│  │  │ (worker) │──│ (allowlist, SSRF guard)│ │ │
│  │  └──────────┘  └────────────────────────┘ │ │
│  └────────────────────────────────────────────┘ │
└─────────────────────────────────────────────────┘
```

New features that require network access must go through one of:
1. `sandbox/proxy/allowlist.rs` (Docker sandbox paths)
2. `tools/builtin/http.rs` `DomainAllowlist` (built-in HTTP tool)
3. `tools/wasm/allowlist.rs` (WASM tool network calls)

### 5.3 Secrets Handling Rules

- All new secrets stored via `secrets/store.rs` + `secrets/crypto.rs` (AES-256-GCM).
- API keys loaded into process memory via `secrecy::Secret<String>`; never cloned into log output.
- New HTTP clients must use `reqwest` with TLS 1.3 minimum (already enforced via `rustls` feature flag).
- Provider API keys must never appear in SSE events, web UI responses, or DB-stored job records.
- New database columns storing sensitive data must use `secrecy` wrappers and be excluded from `SELECT *` queries.

### 5.4 Prompt Injection Guardrails for New Code

Every new code path that injects external content into the LLM context must:
1. Call `Sanitizer::sanitize(&content)` from `src/safety/sanitizer.rs`.
2. Validate via `Validator::validate(&content)` from `src/safety/validator.rs`.
3. Apply `PolicyRule` evaluation from `src/safety/policy.rs`.
4. Check `LeakDetector::scan(&content)` before any outbound delivery.

New channels (WhatsApp, Matrix, etc.) must wire these four calls in `wasm/wrapper.rs` before the content reaches the agent loop.

---

## 6. Implementation Strategy

### 6.1 Architectural Decisions

| Decision | Rationale |
|---|---|
| **Rust throughout** | Memory safety, single binary, performance; no Node.js runtime dependency |
| **WASM channels over native** | Capability isolation; hot-activate without restart; Ed25519 manifest verification |
| **Dual DB backend** | `postgres` feature for production; `libsql` for zero-dependency local mode |
| **rig-core for LLM abstraction** | Handles OpenAI-compatible providers; new providers implemented as `rig` adapters where possible |
| **NEAR AI as primary** | Session-based auth with auto-renewal; direct integration without API key for open access |
| **Separate crates for channels** | `channels-src/<name>/` compiled to WASM; main crate has no compile-time dependency on channel logic |
| **No mobile/desktop apps** | Focus server-side; web gateway + TUI cover the target user profile |

### 6.2 Module Ownership

| Domain | Primary Module(s) | Phase |
|---|---|---|
| Security / sandboxing | `src/safety/`, `src/sandbox/`, `src/crypto/` | P0 (done) + Phase 1 |
| LLM providers | `src/llm/` | Phase 2 |
| Messaging channels | `channels-src/`, `src/channels/wasm/` | Phase 1–3 |
| Multi-agent | `src/agent/`, `src/workspace/` | Phase 2 |
| Web UI / frontend | `src/channels/web/static/` | Phase 2–3 |
| Automation / hooks | `src/agent/routine.rs`, `src/agent/routine_engine.rs` | Phase 1–3 |
| Gateway serviceability | `src/channels/web/`, `deploy/` | Phase 1 |
| Developer experience | `deploy/`, CI `.github/workflows/` | Phase 3 |

### 6.3 Migration and Refactor Plan

#### Database Schema Migrations
- All schema changes in `migrations/` (PostgreSQL) or `src/db/libsql_migrations.rs` (libSQL).
- Migrations are idempotent (`CREATE TABLE IF NOT EXISTS`, `ALTER TABLE … ADD COLUMN IF NOT EXISTS`).
- New feature branches must include a migration file if they add or alter tables.
- Breaking schema changes require a versioned migration and must be listed in `CHANGELOG.md`.

#### API Stability
- The OpenAI-compatible `/v1/chat/completions` endpoint is stable; breaking changes require a version bump in the path.
- Internal `Database` trait additions require implementing stub methods on both `PostgresBackend` and `LibSqlBackend` in the same PR.
- `Channel` trait changes are additive only; breaking changes require a deprecation cycle.

#### WASM Channel ABI
- `wit/` directory defines the WIT interface; changes require a semver bump in channel WASM manifests.
- Existing WASM channels must be re-compiled and re-published when WIT interface changes.

### 6.4 Branching Strategy

```
main                          ← stable releases
  └── dev-local               ← integration branch (active development base)
        └── feat/<area>/<item> ← feature branches (short-lived)
        └── fix/<issue>       ← bug/security fixes
        └── sec/<cve>         ← security-only patches (may go direct to main)
```

- Feature branches merge to `dev-local` via PR; `dev-local` merges to `main` on release.
- Security patches that fix vulnerabilities in released code may bypass `dev-local` and merge directly to `main`.
- Branch names must be kebab-case, prefixed with `feat/`, `fix/`, `sec/`, `docs/`, or `chore/`.

---

## 7. Testing and Release Plan

### 7.1 Test Strategy

| Layer | Tool | Scope | Requirement |
|---|---|---|---|
| Unit tests | `cargo test` | All `src/` modules | Every new public function has at least one unit test |
| Integration tests | `cargo test` in `tests/` | DB, workspace, agent loop | New features touching DB or agent loop add integration test |
| Security regression | Custom test fixtures | Prompt injection, SSRF, path traversal | Any security fix adds a regression test in `tests/security/` |
| Fuzz targets | `cargo-fuzz` | Sanitizer, validator, WASM loader, JSON parser | P1: sanitizer/validator fuzz targets; P2: WASM loader |
| Clippy | `cargo clippy --all --all-features` | All code | Zero warnings; enforced in CI |
| Format | `cargo fmt --check` | All code | Enforced in CI |
| Coverage | `cargo-tarpaulin` or `llvm-cov` | All code | Target ≥ 70% line coverage on `src/safety/` |

### 7.2 CI Pipeline (`.github/workflows/`)

Current CI gates (must not regress):
1. `cargo fmt --check`
2. `cargo clippy --all --benches --tests --examples --all-features`
3. `cargo test`
4. `cargo build --release` (sanity check)

Additions planned per phase:

**Phase 1**:
- `cargo fuzz run sanitizer_fuzz -- -max_total_time=60` (nightly CI job)
- Secret scan on staged diff (via `trufflehog` or `gitleaks` GitHub Action)
- `cargo deny check` for license and advisory audits

**Phase 2**:
- Integration test matrix against PostgreSQL 15 and libSQL (both backends)
- WASM channel build and signature verification smoke test

**Phase 3**:
- Pre-commit hook scaffold (`pre-commit` config with `cargo fmt`, `cargo clippy`, secret scan)
- End-to-end smoke test (gateway start → send message → check SSE response)

### 7.3 Release Cadence

| Stage | Trigger | Process |
|---|---|---|
| Patch release | Security fix or critical bug | Branch from `main`; hotfix PR; `release-plz` bumps patch version; binary artifacts via `cargo-dist` |
| Minor release | Phase milestone complete on `dev-local` | PR from `dev-local` to `main`; update `CHANGELOG.md`; tag; `cargo-dist` release |
| Major release | Breaking API/DB/ABI change | RFC process first; announce deprecations in prior minor; phased rollout |

### 7.4 Fuzz Targets Priority

| Target | File | Rationale |
|---|---|---|
| `sanitizer_fuzz` | `src/safety/sanitizer.rs` | Primary injection defense; high value |
| `validator_fuzz` | `src/safety/validator.rs` | Input validation gateway |
| `wasm_loader_fuzz` | `src/tools/wasm/loader.rs` | Untrusted binary parsing |
| `json_tool_fuzz` | `src/tools/builtin/json.rs` | Arbitrary JSON from tool results |
| `skill_parser_fuzz` | `src/skills/parser.rs` | SKILL.md YAML frontmatter parsing |

---

## 8. Risk Register

| Risk | Likelihood | Impact | Mitigation | Owner Role |
|---|---|---|---|---|
| **rig-core upstream breaking change** | Medium | High | Pin rig-core version; maintain fork-compatible shim layer in `llm/rig_adapter.rs`; monitor rig changelog | LLM Engineer |
| **WASM WIT ABI churn** (wasmtime releases) | Medium | Medium | Pin wasmtime version; version-gate WASM channels with `min_host_version` in manifest | Platform Engineer |
| **New provider rate limits / auth changes** | High | Low-Medium | `FailoverProvider` + `CircuitBreaker` absorbs transient failures; provider-specific error classification | LLM Engineer |
| **Channel platform API deprecations** (Telegram, Slack, Discord) | Medium | High | WASM channel isolation means a single channel update doesn't affect core; monitor API changelogs | Channel Engineer |
| **Contributor velocity slowing** after P0/P1 (low-hanging fruit exhausted) | Medium | Medium | Clear milestone tasks in this doc; `FEATURE_PARITY.md` ownership model; dev/issue templates | Project Lead |
| **Security regression in new channel code** | Medium | High | Phase 1 security hardening first; all new channels run through sanitizer/validator/leak-detector pipeline; security CI gate | Security Engineer |
| **Database migration conflict** (concurrent contributors) | Low | Medium | Sequential migration numbering; PR checklist requires migration review; libSQL migration synced with PG | Platform Engineer |
| **Large WASM binary size** (new channels) | Low | Low | Fuel metering already enforced; `wasm-opt` pass in CI for channel builds | Platform Engineer |
| **Cross-compilation breakage** (Windows, Apple Silicon) | Low | Medium | `cargo-dist` matrix already covers 4 targets; new platform-specific code guarded by `cfg` | Release Engineer |

---

## 9. Upgrade and Compatibility Plan

### 9.1 Database Compatibility

- **PostgreSQL**: Migrations use `refinery`; backward-compatible (additive) changes only within a minor version; breaking changes require a major version bump and documented upgrade steps in `CHANGELOG.md`.
- **libSQL**: Migrations in `src/db/libsql_migrations.rs`; same additive-only constraint; tests run against both backends in CI (Phase 2).

### 9.2 WASM Channel Compatibility

- Installed WASM channels are validated against the current WIT interface version at activation time.
- When the WIT interface changes, `src/channels/wasm/wrapper.rs` must continue to load old channels with a compatibility shim until the next major release.
- The embedded registry catalog (`channels-src/`) pins channel WASM versions; users can upgrade channels with `ironclaw channels upgrade <name>`.

### 9.3 Config File Compatibility

- `config.toml` and `settings.json` fields are additive; removed fields are ignored with a deprecation log warning.
- New required fields must have defaults to avoid breaking existing installations.
- `ironclaw doctor` will flag unknown config keys as warnings.

### 9.4 CLI Backwards Compatibility

- Existing subcommands (`run`, `onboard`, `tui`, `config`, `status`, `memory`, `skills`, `pairing`, `gateway`, `channels`, `cron`, `hooks`, `doctor`, `sandbox`) are stable.
- New subcommands are additive.
- Renamed subcommands require a one-release deprecation cycle with an alias.

### 9.5 API Backwards Compatibility

- The OpenAI-compatible `/v1/chat/completions` endpoint is versioned and stable.
- Internal REST API endpoints under `/api/` are not considered public API; they may change between minor versions but changes will be documented.
- SSE event schemas are additive; new fields will not break existing consumers.

---

## Contributing to the Roadmap

1. **Claim a Phase 1/2/3 item**: Edit the relevant table and add your GitHub handle as owner in the row.
2. **Open a tracking issue**: Reference this document and link the issue to the table row.
3. **Branch from `dev-local`**: Use the `feat/<area>/<item>` naming convention.
4. **Update `FEATURE_PARITY.md`**: Change status from ❌ → 🚧 when you start; ✅ when complete.
5. **PR to `dev-local`**: Include security checklist (see `AGENTS.md`) and migration file if applicable.
6. **Update this document**: If your work closes a milestone item, mark it complete with a ✅ and the PR number.

---

_This document lives at the root of the repository and is updated as part of every feature PR that advances a roadmap item. It is not a static snapshot—treat it as a living specification._
