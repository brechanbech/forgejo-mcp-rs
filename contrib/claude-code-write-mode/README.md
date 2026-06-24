# Claude Code write-mode indicator (personal convenience tooling)

A small, optional add-on that shows an orange **`● FORGEJO WRITE · Nm`** segment in the
[Claude Code](https://claude.com/claude-code) status line whenever this MCP server's
time-boxed **write mode** is active, counting down and disappearing the moment it auto-reverts.

> **Not part of the server.** This is optional client-side config for *one specific MCP client*
> (Claude Code). It does nothing for `cargo install` users, other MCP clients, or the crate
> itself — MCP servers cannot drive client UI. Use it if you run this server under Claude Code
> and want the indicator; otherwise ignore this directory.

## How it works

MCP gives a server no way to colour the client, so the indicator is assembled entirely on the
client side from two documented Claude Code features:

1. A **`PostToolUse` hook** matches the forgejo write-mode tools and runs `write-mode-hook.sh`,
   which writes the write-mode **expiry** (epoch seconds) to `/tmp/forgejo-write-mode-<session>`.
2. A **status line** command (`statusline.sh`) reads that file every few seconds and renders the
   coloured segment while `now < expiry`, deleting the file once it lapses.

Because the window is stored as an absolute expiry and the status line re-checks it on a timer,
the indicator **self-clears when write mode auto-reverts** — even though no tool call fires at
that moment. State is keyed by `session_id`, so concurrent sessions stay independent.

Both scripts are pure POSIX `sh` (no `jq`) and work on macOS, FreeBSD, and Linux.

## Install (per machine)

From a clone of this repo:

```sh
mkdir -p ~/.claude/forgejo-mcp
cp contrib/claude-code-write-mode/statusline.sh ~/.claude/forgejo-mcp/
cp contrib/claude-code-write-mode/write-mode-hook.sh ~/.claude/forgejo-mcp/
chmod +x ~/.claude/forgejo-mcp/statusline.sh ~/.claude/forgejo-mcp/write-mode-hook.sh
```

Then merge the keys from [`settings.snippet.json`](settings.snippet.json) into
`~/.claude/settings.json` (keep your existing keys; add `statusLine` and `hooks`). The snippet
references the fixed `~/.claude/forgejo-mcp/` path, so the same `settings.json` works on every
machine. Restart Claude Code (or it picks up settings on the next session).

To verify without entering real write mode:

```sh
# Fake an active 9-minute window for a test session, then render the status line.
echo "$(( $(date +%s) + 540 ))" > /tmp/forgejo-write-mode-test
printf '{"session_id":"test","workspace":{"current_dir":"%s"}}' "$PWD" \
  | ~/.claude/forgejo-mcp/statusline.sh
rm -f /tmp/forgejo-write-mode-test
```

## Notes

- The countdown is **minute-granular** — that's the resolution the server's write-mode note
  exposes.
- `refreshInterval` is in **seconds** (minimum 1); 5 is a reasonable default.
- If new write tools are added to the server, add them to the hook `matcher` so the window keeps
  sliding the indicator forward on those calls.
- **Payload escaping:** Claude Code delivers an MCP tool's result to `PostToolUse` as a
  *JSON-escaped string* inside a `tool_response` array — so the server's JSON reaches the hook as
  e.g. `\"minutes_remaining\": 9` (backslash-escaped quotes, whitespace after the colon). The
  hook's matchers therefore anchor on the key *name* and bridge to the number with a non-digit
  run, rather than matching a literal `"minutes_remaining":`. Keep this in mind if you adapt the
  patterns: don't assume the keys appear unescaped.
- Uninstall: delete `~/.claude/forgejo-mcp/` and remove the `statusLine`/`hooks` keys.
