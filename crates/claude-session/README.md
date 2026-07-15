# claude-session

Stop multiple Claude Code sessions in the **same working directory** from
clashing — corrupted git index, stomped commits, `--resume` picking the wrong
session. Two layers:

- **Registry + detection** — each session writes its own record
  (`~/.claude/sessions/<sid>.json`); `list`/`guard` flag any repo whose *live*
  working tree is claimed by 2+ active sessions.
- **Worktree isolation** — `isolate` moves a session into its own git worktree
  (separate index + files, shared history, a `session/<sid8>` branch) so work
  can't collide; `release` tears it down.

## Use

```sh
claude-session register            # record this session (repo, pwd, transcript)
claude-session list                # all sessions; flags live-tree collisions
claude-session isolate             # give THIS session its own worktree, then cd into it
claude-session guard               # exit 3 if another active session shares this live tree
claude-session release             # remove this session's worktree (refuses if dirty)
```

Session id resolves from `--session-id`, else `$CLAUDE_CODE_SESSION_ID`, else the
newest transcript. It's validated to the UUID charset before it ever touches a
path.

## Recommended workflow

When you're about to run a second session against a repo another session is
already in:

```sh
claude-session guard || claude-session isolate && cd "$(...)"   # isolate on collision
```

`guard` is exit-code driven so it drops into a `PreToolUse` or pre-commit hook:
warn (or block) before two sessions commit to one tree.

## Relationship to the Agent OS

Complements `SESSION-PROTOCOL.md` (the Agent OS session bootstrap + the
`agent-registry` of projects/goals/tasks). That governs *what* a session works
on and how it boots; this governs the session **process and working tree**.
Natural next step: call `claude-session register` from the SessionStart hook and
`claude-session guard` before commits, so isolation is automatic.

## State

- `~/.claude/sessions/<sid>.json` — one record per session (per-file, so the
  registry is never itself a point of contention).
- `~/.claude/worktrees/<repo>/<sid8>/` — isolated worktrees.
