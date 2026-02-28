Launch a visible sub-agent in a new tmux window (tab) to perform a task.

Task: $ARGUMENTS

## Instructions

1. Formulate a specific, detailed prompt for the sub-agent based on the task requested. Include all necessary context.
2. Write that prompt to a temporary file (e.g., `/tmp/sub-agent-task.txt`).
3. Determine a short, descriptive name for the tab (no spaces, e.g., "agent-docs" or "agent-build").
4. Execute the following command to open a new tab and start the agent automatically:
   `tmux new-window -n "<tab-name>" "claude -p \"\$(cat /tmp/sub-agent-task.txt)\""`

## Workflow

Once the command is executed, inform the user that the tab has been created and they can view it. Remind them to use `Ctrl+b` then `n` to switch to it.
