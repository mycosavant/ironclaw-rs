## Plan: Addressing IronClaw Review Findings

### Wave 1 — Critical/High, Small Effort (all S) · _Do first_

These are the highest-severity issues with the smallest fix scope. All are in-place fixes with no architectural change required.

| ID                    | Finding                                                                                   | Change location                        | CLAUDE guideline |
| --------------------- | ----------------------------------------------------------------------------------------- | -------------------------------------- | ---------------- |
| **C-1**               | `wrap_external_content()` never called — raw LLM injection path open                      | `agent/agent_loop.rs`                  | QW-1             |
| **H-2**               | Proxy binds `127.0.0.1`, containers connect to `172.17.0.1` → allowlist bypassed on Linux | http.rs                                | QW-2             |
| **H-4**               | 3× `.expect()` panic sites in production tool code                                        | job.rs, `tools/mcp/client.rs`, http.rs | QW-3             |
| **M-2**               | `?token=` SSE param not URL-decoded before constant-time compare                          | `channels/web/auth.rs`                 | —                |
| **M-3**               | Rate-limiter window-reset race (3 relaxed atomics, no CAS)                                | `channels/web/server.rs`               | —                |
| **M-4 = H-1 partial** | `base64 -D` (macOS), `$IFS` / tab bypass of shell pre-filter                              | `tools/builtin/shell.rs`               | QW-5             |
| **M-5**               | WASM compilation cache unbounded (no eviction, 5–50 MB per module)                        | `tools/wasm/runtime.rs`                | QW-9             |
| **M-6**               | Proxy `reqwest::Client` has no pool limit → FD exhaustion                                 | http.rs                                | —                |
| **L-2**               | CORS origin is `http://0.0.0.0:PORT` when binding to wildcard                             | `channels/web/server.rs`               | —                |
| **L-6**               | `NODE_PATH` in `SAFE_ENV_VARS` enables Node module hijack                                 | `tools/builtin/shell.rs`               | —                |

---

### Wave 2 — High/Medium, Medium Effort · _Tackle after Wave 1_

| ID                  | Finding                                                                                                        | Change location                                                                                   |
| ------------------- | -------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------- |
| **H-1 (remaining)** | Full `$IFS`/tab shell bypass fix — normalize whitespace pre-filter, block `${}` / backtick in non-sandbox path | `tools/builtin/shell.rs`                                                                          |
| **M-1**             | Unicode homoglyph bypass of Aho-Corasick sanitizer                                                             | `safety/sanitizer.rs` + Cargo.toml (`unicode-normalization`)                                      |
| **H-3**             | Bearer token in SSE URL (`?token=`) — add one-time ticket endpoint                                             | `channels/web/server.rs`, `channels/web/auth.rs`                                                  |
| **M-7**             | `EnvCredentialResolver` exposes all host env vars; replace with `SecretsStore` resolver                        | http.rs, `sandbox/manager.rs`                                                                     |
| **L-4**             | No liveness indicators for background tasks (heartbeat, routine engine, self-repair)                           | `agent/heartbeat.rs`, `agent/routine_engine.rs`, `agent/self_repair.rs`, `channels/web/server.rs` |

---

### Wave 3 — Low / Performance · _Polish pass_

| ID        | Finding                                                                                  | Effort |
| --------- | ---------------------------------------------------------------------------------------- | ------ |
| **L-1**   | Escape content at High+ severity, not only Critical                                      | S      |
| **L-3**   | WASM `tables_created`/`instances_created` dead-code or implement                         | S      |
| **L-5**   | Document + test `update_conversation_metadata_field` merge-patch mismatch (libSQL vs PG) | S      |
| **P-1**   | Replace O(n) LRU eviction in `CachedProvider` with `lru` crate                           | S      |
| **P-2**   | Batch `get_jobs_by_ids` to avoid N+1 in `self_repair.rs`                                 | M      |
| **QW-10** | Add `cargo audit` to CI                                                                  | S      |

---

### Wave 4 — High-Value Features · _After security baseline is green_

Ordered by impact/effort ratio:

1. **HVF-3** — Prompt injection circuit breaker with quarantine mode (M) — closes the gap that Wave 1 mitigates but doesn't fully solve
2. **HVF-4** — SSRF-protected HTTP tool with domain allowlist (M) — current `HttpTool` is essentially an open SSRF vector
3. **HVF-2** — Per-session short-lived gateway tokens (M) — replaces the single long-lived token that H-3 partially patches
4. **HVF-1** — Encrypted-at-rest libSQL via SQLite SEE (M) — closes the plaintext-on-disk gap noted in CLAUDE.md
5. **HVF-5** — Workspace memory access control per skill trust (L) — completes the trust attenuation model
6. **HVF-6** — Cryptographic integrity verification for installed WASM tools (L) — supply-chain protection

---

### Summary view

```
Wave 1  ── 10 fixes, all S effort, closes C-1 + H-2 + H-4 fully
Wave 2  ── 5 fixes, S/M effort, closes H-1 + H-3 + M-1 + M-7 + L-4
Wave 3  ── 6 items, mostly S, polish + CI hygiene
Wave 4  ── 6 features, M/L effort each
```

**Recommended starting point:** Wave 1 in a single PR — all 10 changes are small, self-contained, and each one either closes a confirmed exploit path or prevents a process-killing panic. Want me to start implementing Wave 1 now?

Completed: _Analyze review findings_ (1/2)
