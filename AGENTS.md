# Agent Rules

> **Active development base:** `dev-local`  
> All work targeting the OpenClaw parity roadmap branches from and merges back to `dev-local`.

---

## 1. Feature Parity Update Policy

- If you change implementation status for any feature tracked in `FEATURE_PARITY.md`, update that file in the same branch.
- Do not open a PR that changes feature behavior without checking `FEATURE_PARITY.md` for needed status updates (`❌`, `🚧`, `✅`, notes, and priorities).
- When closing a roadmap item from `ROADMAP_OPENCLAW_PARITY.md`, mark it ✅ in both `FEATURE_PARITY.md` and `ROADMAP_OPENCLAW_PARITY.md` in the same PR.

---

## 2. Check Before Implementing

Before writing new code for any feature, search the codebase first:

1. `grep -rn 'feature_name\|KeywordFromSpec' src/` -- look for existing implementations
2. Read the relevant module files (e.g., `src/tools/builtin/`, `src/safety/`, `src/db/`) before writing anything
3. If the feature appears to be missing, check adjacent files — it may live in a differently-named module

Multiple features tracked as `❌` in `FEATURE_PARITY.md` were already fully implemented; the only gap was documentation. Writing a duplicate wastes time and risks subtle divergence from the existing implementation.

---

## 3. Branching and PR Hygiene

### Branch naming
```
feat/<area>/<short-description>   # new features
fix/<issue-number>                # bug fixes
sec/<cve-or-short-desc>           # security patches
docs/<topic>                      # documentation only
chore/<topic>                     # build/CI/dependency updates
```

All branches must be cut from `dev-local` (not `main`) unless the change is a security hotfix that must land directly on `main`.

### PR checklist
Every PR must include:
- [ ] `FEATURE_PARITY.md` updated if feature status changed
- [ ] `ROADMAP_OPENCLAW_PARITY.md` updated if a milestone item is completed
- [ ] Migration file added if DB schema changed (`migrations/` for PG, `src/db/libsql_migrations.rs` for libSQL)
- [ ] Security checklist (Section 4) completed
- [ ] `cargo fmt --check` passes
- [ ] `cargo clippy --all --all-features` passes with zero warnings
- [ ] `cargo test` passes
- [ ] `CHANGELOG.md` entry added for user-visible changes

### PR size
Keep PRs focused: one logical change per PR. Large features should be split into smaller, independently mergeable steps (e.g., DB schema → backend logic → API endpoint → UI).

---

## 4. Security Checklist for Every PR

Before opening a PR that adds or modifies code, answer each question. If the answer to any security question is "yes," include notes in the PR description explaining how it is mitigated.

| Question | If Yes |
|---|---|
| Does this add a new inbound data path (channel message, webhook, file, tool output)? | Route through `Sanitizer::sanitize()` + `Validator::validate()` in `src/safety/` |
| Does this send content to an external service (LLM, webhook, API)? | Run `LeakDetector::scan()` on outbound payload |
| Does this make HTTP requests to user-supplied URLs? | Use `DomainAllowlist::check()` from `src/sandbox/proxy/allowlist.rs` or `src/tools/wasm/allowlist.rs` |
| Does this spawn a subprocess or run a shell command? | Call `scrub_env()` to strip secret-pattern env vars; add LD*/DYLD* check |
| Does this load or execute untrusted binary/WASM code? | Verify Ed25519 manifest signature via `crypto/signing.rs` before execution |
| Does this store or return credentials / secrets? | Use `secrecy::Secret<String>`; never log; use `secrets/crypto.rs` for persistence |
| Does this add a new DB column with sensitive data? | Wrap in `secrecy`; exclude from `SELECT *` queries |
| Does this change gateway authentication or session handling? | Add/update tests in `tests/` covering auth bypass scenarios |
| Does this change tool policy enforcement? | Verify policy is checked in `ToolRegistry::resolve()`, not only in the tool itself |
| Does this add a new WASM host function? | Security review required; document capability in `wit/`; add fuzz target |

---

## 5. Using Agent Automation to Execute the Roadmap

IronClaw development uses automated agents (AI coding assistants, CI bots) extensively. This section describes how to use them safely and effectively.

### 5.1 What agents are good for

- **Scaffold boilerplate**: New WASM channel shells, new `LlmProvider` implementations, new tool stubs, migration files.
- **Grep/search**: Finding existing implementations before writing new code (Section 2).
- **Lint and format fixes**: Running `cargo clippy --fix` and `cargo fmt` on a branch.
- **Test generation**: Generating unit test skeletons for new public functions.
- **Documentation**: Updating `FEATURE_PARITY.md` status rows, `CHANGELOG.md` entries.
- **Dependency audits**: Running `cargo deny check` and summarising advisory findings.

### 5.2 What agents must NOT do without human review

- **Merge to `main` directly** — all merges to `main` require a human-approved PR.
- **Modify `src/safety/`** — prompt injection defenses, leak detector, and validator changes must be human-reviewed.
- **Modify `src/crypto/`** — cryptographic code changes (Ed25519 signing, AES key derivation) require Security Engineer review.
- **Modify `src/sandbox/`** — sandbox boundary code changes require Security Engineer review.
- **Rotate or commit secrets** — agents must never write API keys, tokens, or credentials to any file.
- **Disable or bypass security tests** — agents must never skip, remove, or weaken existing security-related test cases.
- **Bump `wasmtime` or `rig-core` major versions** — ABI-breaking dependency bumps require explicit human sign-off.

### 5.3 Validating agent-generated changes

After an agent produces a code change, run the following validation sequence before opening a PR:

```bash
# 1. Format check
cargo fmt --check

# 2. Lint (zero warnings required)
cargo clippy --all --benches --tests --examples --all-features

# 3. Full test suite
cargo test

# 4. Dependency advisory audit
cargo deny check

# 5. Check for accidental secret commits
git diff --cached | grep -E '(api_key|secret|token|password)\s*=' && echo "STOP: potential secret in diff"
```

If any step fails, the agent must fix the issue before the PR is opened.

### 5.4 Agent workflow for a roadmap feature

1. **Read this file and `FEATURE_PARITY.md`** before starting any implementation.
2. **Search the codebase** using `grep -rn` or semantic search for existing implementations of the target feature.
3. **Branch from `dev-local`**: `git checkout dev-local && git checkout -b feat/<area>/<item>`.
4. **Implement the smallest correct change** that satisfies the "Definition of Done" in `ROADMAP_OPENCLAW_PARITY.md`.
5. **Update `FEATURE_PARITY.md`**: change ❌ → 🚧 immediately; → ✅ when DoD is met.
6. **Run the validation sequence** (Section 5.3).
7. **Open a PR to `dev-local`** with the PR checklist (Section 3) completed.
8. **Do not self-merge** — all PRs require at least one human approval.

### 5.5 Security checks in CI

The CI pipeline enforces the following automatically. Agents must not attempt to bypass these:

| Check | Command | Failure action |
|---|---|---|
| Format | `cargo fmt --check` | Fix with `cargo fmt`; do not suppress |
| Lint | `cargo clippy --all --all-features` | Fix all warnings; no `#[allow(clippy::...)]` without justification comment |
| Tests | `cargo test` | Fix failing tests; do not delete tests |
| Advisory | `cargo deny check` (Phase 1+) | Investigate advisory; do not `deny = false` without Security Engineer sign-off |
| Secret scan | `gitleaks` / `trufflehog` (Phase 1+) | Remove secret; rotate if already committed |

---

## 6. Coordination and Ownership

- Each major section in `FEATURE_PARITY.md` and `ROADMAP_OPENCLAW_PARITY.md` has an **Owner role** field.
- Before starting work on a feature, edit the Owner field to add your GitHub handle. This signals intent and prevents duplicate work.
- If you abandon a feature, clear your name from the Owner field so others can pick it up.
- For features with cross-cutting security implications, tag the PR with `security` and request review from someone in the Security Engineer role.

---

## 7. References

- [`FEATURE_PARITY.md`](./FEATURE_PARITY.md) — per-feature status matrix
- [`ROADMAP_OPENCLAW_PARITY.md`](./ROADMAP_OPENCLAW_PARITY.md) — phased milestones and implementation strategy
- [`CONTRIBUTING.md`](./CONTRIBUTING.md) — general contribution guidelines
- [`CLAUDE.md`](./CLAUDE.md) — architecture reference and module map
