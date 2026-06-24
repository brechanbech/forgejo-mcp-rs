#!/bin/sh
# Claude Code PostToolUse hook for the forgejo MCP write-mode tools.
#
# Maintains a per-session state file holding the write-mode expiry as epoch seconds; the
# companion status line (statusline.sh) reads it to show a colored indicator. Pure POSIX sh —
# no jq. The hook receives the tool-call JSON on stdin.
#
# Install: see README.md in this directory. Matched against the forgejo write-mode tools via
# the `matcher` in settings.json, so it only fires on those calls.

input=$(cat)

# Session id keys the state file, so concurrent Claude Code sessions don't cross-contaminate.
session=$(printf '%s' "$input" | sed -n 's/.*"session_id":"\([^"]*\)".*/\1/p' | head -1)
[ -n "$session" ] || exit 0
state="/tmp/forgejo-write-mode-${session}"

# Explicitly inactive (disable_write_mode result, or a write tool's "write mode inactive"
# note) → clear the indicator.
if printf '%s' "$input" | grep -q '"write_mode_active":false' \
   || printf '%s' "$input" | grep -q 'write mode inactive'; then
	rm -f "$state"
	exit 0
fi

# Pull the remaining/window minutes from whichever shape the server emitted:
#   - write tools append "... about N min remaining ..."
#   - write_status returns "minutes_remaining": N
#   - enable_write_mode returns "minutes": N   (the window length)
# First match wins; the patterns are ordered most- to least-specific.
mins=$(printf '%s' "$input" | sed -n 's/.*about \([0-9][0-9]*\) min remaining.*/\1/p' | head -1)
[ -n "$mins" ] || mins=$(printf '%s' "$input" | sed -n 's/.*"minutes_remaining":\([0-9][0-9]*\).*/\1/p' | head -1)
[ -n "$mins" ] || mins=$(printf '%s' "$input" | sed -n 's/.*"minutes":\([0-9][0-9]*\).*/\1/p' | head -1)

if [ -n "$mins" ] && [ "$mins" -gt 0 ]; then
	now=$(date +%s)
	printf '%s\n' "$(( now + mins * 60 ))" > "$state"
fi
exit 0
