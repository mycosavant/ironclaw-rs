# Workflow & Quality Discipline

## Ship Gate

Before considering any implementation complete, run the full quality gate:

1. `cargo fmt` — format all code
2. `cargo clippy --all --benches --tests --examples --all-features` — zero warnings, including pre-existing ones
3. `cargo test` — all tests pass
4. `grep -rnE '\.unwrap\(|\.expect\(' <changed files>` — no panics in production code (tests are fine)
5. `grep -rn 'super::' <changed files>` — use `crate::` imports

Do not present work as done until all five checks pass. If a check fails, fix and re-run.

## Code Review

After completing a major implementation (a feature, a multi-file refactor, or a sprint/wave of changes), review the code autonomously using a specialized review agent before declaring it done. Focus on:

- Correctness: edge cases, race conditions, error paths
- Security: injection, data leaks, improper validation
- Consistency: matches existing codebase patterns and conventions
- Completeness: all necessary files updated (both DB backends, all callers, etc.)

_All code must be producton-quality before being merged. If the review finds issues, fix them and re-run the review until it passes._

## Fix the Pattern, Not the Instance

When fixing a bug or addressing review feedback, search the entire codebase for all instances of the same pattern. A fix in one file that doesn't address the same issue elsewhere is incomplete. Use `grep` across `src/` to find siblings.

## Check Before Implementing

Before writing a new feature, grep and read the relevant source files to verify it doesn't already exist. Duplicate implementations waste time and introduce divergence.

## Balance

Apply enterprise-grade practices where they matter (security, safety, correctness) but stay pragmatic. No gold-plating, no over-engineering, no YAGNI violations. Three similar lines of code is better than a premature abstraction. Simple and correct beats clever and fragile.
