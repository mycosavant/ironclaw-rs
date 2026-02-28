# Hardening Sprint — Wave 2 Post-Merge Polish

**Date**: 2026-02-28
**Branch**: `dev-local` (post Wave 2 merge)
**Goal**: Close all deferred items from the `feat/wave2-browser` pre-merge review, address residual tech debt, and fill test coverage gaps in new modules.
**Scope**: Security hardening, API completeness, code cleanup, documentation polish. No new user-facing features.

---

## Prioritized Task List — Deferred Review Items

### 1A-4: CONNECT Tunnel IP Check (P0 — Security, Complexity: S)

**Current state**: In `src/sandbox/proxy/http.rs`, `handle_connect()` calls `TcpStream::connect(&target)` with the raw hostname. The domain allowlist validates the hostname, but after DNS resolution the resulting IP is never checked against `is_disallowed_ip()`. An attacker who controls an allowlisted domain's DNS could point it to `127.0.0.1` or `169.254.169.254` and tunnel HTTPS to the instance metadata endpoint or local services.

**Files to modify**:
- `src/sandbox/proxy/http.rs` — Between the domain allowlist check and `TcpStream::connect()`, resolve the hostname via `tokio::net::lookup_host()`, check all resulting IPs against `is_disallowed_ip()`, and reject if any resolve to a private/reserved IP.

**Implementation approach**: Add DNS resolution before `TcpStream::connect()`. Use the existing `is_disallowed_ip()` from `src/tools/builtin/http.rs` (already `pub(crate)`). Resolve the target, check each IP, and refuse the connection if any IP is disallowed. Connect using the resolved addresses rather than the hostname to close the TOCTOU window.

**Acceptance criteria**:
- Unit test: CONNECT to a hostname that resolves to `127.0.0.1` is rejected
- Unit test: CONNECT to a hostname that resolves to `169.254.169.254` is rejected
- Normal CONNECT to an allowlisted domain with public IP succeeds

---

### 1A-2: Browser SSRF Redirect Check (P0 — Security, Complexity: M)

**Current state**: `validate_url()` in `src/tools/builtin/http.rs` resolves DNS and checks resolved IPs against `is_disallowed_ip()`. However, when the Playwright driver follows a server-side 3xx redirect, the final destination URL is not re-validated. The initial URL passes SSRF checks in `browser.rs`, but the Node.js driver follows redirects opaquely. The driver script at `browser_driver.js` does not report redirect chains back to Rust.

**Files to modify**:
- `src/tools/builtin/browser_driver.js` — Add `finalUrl` (from `response.url()`) to the navigate response
- `src/tools/builtin/browser.rs` — After receiving the navigate response, extract the final URL, re-validate it through `validate_url()` and the domain allowlist. If it fails, close the page/session and return an SSRF error.

**Implementation approach**: Modify the JS driver to return `finalUrl` in the navigate response. In `BrowserNavigateTool::execute()`, after `parse_driver_response`, extract the final URL. If it differs from the requested URL, run it through `validate_url()` and the domain allowlist. If it fails, send a "close" command to the driver and return `BrowserError::SsrfBlocked`.

**Acceptance criteria**:
- Navigate to a URL that 302-redirects to `http://169.254.169.254/` returns `SsrfBlocked`
- Navigate to a URL that 302-redirects to an off-allowlist domain is blocked when allowlist is configured
- Test that captures the final-URL-check logic

---

### 1A-5: Quarantine Exit Safety (P1 — Security, Complexity: S)

**Current state**: In `src/agent/quarantine.rs`, `try_exit()` lifts quarantine on exact match of `/unquarantine`. In `thread_ops.rs`, `try_exit(content)` is called with the full user message content. The design explicitly requires the message to be EXACTLY the exit command (`.trim()` comparison), so embedding it in longer text will not trigger exit. This is documented behavior, not a bypass.

**Files to modify**:
- `src/agent/quarantine.rs` — Add doc comment on `try_exit()` documenting the intentional trim-exact-match design
- `src/agent/thread_ops.rs` — Add comment noting `content` is raw user input, not LLM output

**Implementation approach**: Documentation-only change. Add a test that verifies `/unquarantine` embedded in a longer string does NOT lift quarantine.

**Acceptance criteria**:
- Test: `try_exit("Please /unquarantine the session")` returns `false`
- Doc comments updated to clarify the intentional design

---

### 1A-6: Leak Detector `add_pattern()` Prefix Matcher Stale (P1 — Code Quality, Complexity: S)

**Current state**: In `src/safety/leak_detector.rs`, `add_pattern()` pushes a new `LeakPattern` to `self.patterns` but does NOT rebuild `self.prefix_matcher`. The comment acknowledges this. Dynamically added patterns with a literal prefix >= 3 chars will not benefit from the Aho-Corasick fast-path scan. They WILL still be checked via the `candidate_indices` fallback, so the scan is functionally correct, just slower.

**Files to modify**:
- `src/safety/leak_detector.rs` — Rebuild the prefix matcher in `add_pattern()`

**Implementation approach**: After pushing the new pattern, rebuild `prefix_matcher` and `known_prefixes` from the full `patterns` vec. This is a cold-path operation (patterns are added at startup, not per-request). Extract the rebuild logic from `with_patterns()` into a private `rebuild_prefix_matcher()` method.

**Acceptance criteria**:
- `add_pattern()` rebuilds the prefix matcher
- Test: add a pattern with a known prefix, then scan content containing that prefix — the new pattern matches
- Test: `pattern_count()` increments after `add_pattern()`

---

### 1C-2: Workflow REST Endpoints (P2 — Feature Completeness, Complexity: M)

**Current state**: Workflows are only manageable via LLM tool calls. The web gateway has no `/api/workflows` routes. The Database trait already provides all needed CRUD methods. `GatewayState` already holds `store: Option<Arc<dyn Database>>`.

**Files to modify**:
- `src/channels/web/server.rs` — Add REST endpoints mirroring workflow tools
- `src/channels/web/types.rs` — Add request/response types
- `src/channels/web/rbac.rs` — Add workflow-specific permissions

**Implementation approach**: Add 6 routes following the existing routines pattern: `GET /api/workflows` (list), `POST /api/workflows` (create), `GET /api/workflows/{id}` (detail), `PUT /api/workflows/{id}` (update), `DELETE /api/workflows/{id}` (delete), `POST /api/workflows/{id}/run` (trigger). Each handler: extract role, check permission, call DB method. Add `ManageWorkflows` and `ViewWorkflows` to RBAC.

**Acceptance criteria**:
- All 6 REST endpoints respond correctly with proper RBAC enforcement
- JSON response shapes documented in types.rs
- Viewer role can list/view, Admin can create/update/delete/run

---

### 1C-3: Routine Trigger Bypasses Engine (P2 — Correctness, Complexity: M)

**Current state**: `routines_trigger_handler()` in the web server dispatches a routine trigger by sending it as a chat message through `msg_tx`. This bypasses `RoutineEngine::fire_manual()`, which properly records the run, checks concurrency limits, and tracks execution. `GatewayState` does not hold a reference to `RoutineEngine`.

**Files to modify**:
- `src/channels/web/server.rs` — Add `routine_engine: Option<Arc<RoutineEngine>>` to `GatewayState`; update handler to call `engine.fire_manual()`
- `src/agent/routine_engine.rs` — Verify `fire_manual()` is callable from gateway context (it already is)
- Startup wiring (`src/main.rs` or gateway builder) — Pass `RoutineEngine` Arc into `GatewayState`

**Implementation approach**: Add `pub routine_engine: Option<Arc<RoutineEngine>>` to `GatewayState`. In `routines_trigger_handler`, if `Some`, call `fire_manual()`. If `None`, fall back to chat-message dispatch with a warning log. Clean upgrade path.

**Acceptance criteria**:
- Triggering a routine via REST API calls `fire_manual()` directly
- Routine run is recorded in the database
- Concurrency limits are enforced
- Fallback to chat-message dispatch when engine is not available

---

### 1C-4: Missing Env Vars in .env.example (P2 — Documentation, Complexity: S)

**Current state**: `.env.example` is missing: `GATEWAY_ENABLED`, `GATEWAY_HOST`, `GATEWAY_PORT`, `GATEWAY_AUTH_TOKEN`, `GATEWAY_USER_ID`, `ROUTINES_*`, `SKILLS_*`, `EMBEDDING_*`, `CLAUDE_CODE_*`, `TINFOIL_*`, `OPENAI_API_KEY`.

**Files to modify**:
- `.env.example` — Add commented-out entries for all missing vars

**Implementation approach**: Add sections for Web Gateway, Routines, Skills, Embeddings, Claude Code, and Tinfoil, matching the CLAUDE.md Configuration section format.

**Acceptance criteria**:
- Every env var in CLAUDE.md's Configuration section has a corresponding `.env.example` entry
- Entries are commented out with defaults shown
- Grouped by logical category

---

## Additional Hardening Items

### H1: Workflow Tool Tests (P1, Complexity: M)

`src/tools/builtin/workflow.rs` has zero tests. Add parameter validation tests for all 6 workflow tools (missing required fields, invalid JSON, non-existent workflow names).

**Acceptance criteria**: At least 12 tests covering parameter validation for all 6 tools.

### H2: Browser Selector Validation Tests (P2, Complexity: S)

`validate_selector()` in `src/tools/builtin/browser.rs` has no dedicated tests. It rejects empty selectors, long selectors, XPath, and Playwright internal selectors.

**Acceptance criteria**: 6+ tests covering all branches of `validate_selector()`.

### H3: `TreeQuery.depth` Dead Code (P2, Complexity: S)

At `src/channels/web/server.rs`, the `depth` field on `TreeQuery` has `#[allow(dead_code)]`. Either wire up depth filtering or remove the field.

### H4: NearAI Chat Response Dead Code (P2, Complexity: S)

At `src/llm/nearai_chat.rs`, three fields have `#[allow(dead_code)]`: `ChatCompletionResponse.id`, `ChatCompletionResponseMessage.role`, `ChatCompletionToolCall.call_type`. Replace with `_`-prefixed field names.

### H5: `TelegramUser.first_name` Dead Code (P2, Complexity: S)

At `src/setup/channels.rs`, `TelegramUser.first_name` has `#[allow(dead_code)]`. Prefix with `_`.

### H6: `agent_spawn` Child Count TOCTOU (P2, Complexity: S)

TOCTOU race in `AgentSpawnTool::execute()` where `active_child_count()` and `dispatch_job()` are not atomic. Add `child_count: AtomicU32` to `JobContext`, use `fetch_add`/`compare_exchange`.

### H7: Workflow Executor Cycle Guard (P2, Complexity: M)

The workflow executor's `execute_tool()` bypasses the SHA-256 cycle guard. Add an optional `CycleGuard` field to `WorkflowExecutor` and check for cycles in `execute_tool()`.

---

## Suggested Ordering

### Batch 1: Security Hardening (P0)
1. 1A-4: CONNECT tunnel IP check
2. 1A-2: Browser SSRF redirect check

### Batch 2: Code Quality + Safety (P1)
3. 1A-6: Leak detector `add_pattern()` rebuild
4. 1A-5: Quarantine exit safety docs + test
5. H1: Workflow tool tests

### Batch 3: API Completeness (P2)
6. 1C-3: Routine trigger engine integration
7. 1C-2: Workflow REST endpoints

### Batch 4: Cleanup + Documentation (P2)
8. 1C-4: Missing env vars in .env.example
9. H2: Selector validation tests
10. H3–H5: Dead code cleanup
11. H6: agent_spawn child count TOCTOU
12. H7: Workflow executor cycle guard

**After each batch**: Run the full quality gate (`cargo fmt`, `cargo clippy --all --all-features`, `cargo test`).

---

## Out of Scope

- **New features**: No new tools, channels, providers, or user-facing capabilities
- **Major refactors**: No rewriting of the agent loop, session model, or database layer
- **libSQL feature gaps**: Workspace migration, secrets store migration, vector search — separate project
- **MCP stdio transport**: Known limitation, separate effort
- **WIT bindgen integration**: Known limitation, separate effort
- **stereOS backend tests**: VM backend needs integration testing but is a separate sprint
- **Integration/E2E tests**: Testcontainers or full E2E browser tests are a separate initiative
- **M10 (Workflow audit trail integration)**: Requires significant architectural changes — deferred
- **Performance optimization**: No profiling or optimization work
- **UI changes**: No modifications to the web gateway static assets
