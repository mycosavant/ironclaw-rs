Launch a visible sub-agent in a new tmux window (tab) to perform a task.

Task: $ARGUMENTS

## Instructions

Note: The task above may reference something discussed earlier in the
conversation (e.g., "do the thing we talked about", "review that module").
Include all relevant context from the conversation in the prompt you write.

1. Formulate a specific, detailed prompt for the sub-agent based on the task
   requested. Include all necessary context from the conversation.
2. Write that prompt to a temporary file at `/tmp/subagent-<tab-name>.md`.
3. Determine a short, descriptive tab name (kebab-case, e.g., "review-safety",
   "trace-browser").
4. Execute:
   ```
   tmux new-window -n "<tab-name>" "claude -p \"$(cat /tmp/subagent-<tab-name>.md)\"; echo ''; echo '--- Agent finished. Press Enter to close ---'; read"
   ```

The trailing `read` keeps the pane open so the user can see the final output
before the tab closes.

## Context Rules

- If the task references files or code discussed in this conversation, include
  the specific file paths and relevant details in the prompt
- If the task involves reading/writing files, use absolute paths since the
  sub-agent shares the same working directory
- The sub-agent runs in the same repo checkout — do NOT use this for tasks that
  write conflicting code changes (use `/worktree` for that)
- Keep prompts self-contained: the sub-agent has no memory of this conversation

## Workflow

After launching, inform the user:

- Which tab was created and its name
- `Ctrl+b n` / `Ctrl+b p` to switch tabs (or click the tab name in the bottom bar)
- The tab will stay open after the agent finishes so they can read the output
