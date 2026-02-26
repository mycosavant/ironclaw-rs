# Agent Rules

## Feature Parity Update Policy

- If you change implementation status for any feature tracked in `FEATURE_PARITY.md`, update that file in the same branch.
- Do not open a PR that changes feature behavior without checking `FEATURE_PARITY.md` for needed status updates (`❌`, `🚧`, `✅`, notes, and priorities).

## Check Before Implementing

Before writing new code for any feature, search the codebase first:

1. `grep -rn 'feature_name\|KeywordFromSpec' src/` -- look for existing implementations
2. Read the relevant module files (e.g., `src/tools/builtin/`, `src/safety/`, `src/db/`) before writing anything
3. If the feature appears to be missing, check adjacent files — it may live in a differently-named module

Multiple features tracked as `❌` in `FEATURE_PARITY.md` were already fully implemented; the only gap was documentation. Writing a duplicate wastes time and risks subtle divergence from the existing implementation.
