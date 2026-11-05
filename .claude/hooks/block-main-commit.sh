#!/usr/bin/env bash
cmd=$(jq -r '.tool_input.command // empty')

commit_re='(^|[;&|(]|[[:space:]])git([[:space:]]+-[cC][[:space:]]+[^[:space:]]+)*[[:space:]]+commit([[:space:]]|$)'
if [[ ! "$cmd" =~ $commit_re ]]; then
  echo '{}'
  exit 0
fi

branch=$(git branch --show-current 2>/dev/null)
if [ "$branch" = "main" ]; then
  jq -n '{"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "deny", "permissionDecisionReason": "Direct commits to main are not allowed - this repo uses git-flow-next (see CLAUDE.md); main only receives merges from release/* or hotfix/* branches. Commit to develop/feature/bugfix instead, or use git-flow release/hotfix start."}}'
  exit 0
fi

echo '{}'
