#!/bin/sh
# Claude Code status line: a quiet context line (dir + git branch), plus an orange
# "● FORGEJO WRITE · Nm" segment (minute-granular countdown) whenever this session's forgejo
# MCP write mode is active.
#
# Reads the per-session expiry written by write-mode-hook.sh, and self-clears when the window
# lapses (write mode auto-reverts server-side with no tool call to observe, so the status line
# must expire it on its own). Pure POSIX sh — no jq. Claude Code passes the session JSON on
# stdin and renders this script's stdout. Needs a 256-color terminal for the orange.

input=$(cat)

session=$(printf '%s' "$input" | sed -n 's/.*"session_id":"\([^"]*\)".*/\1/p' | head -1)
dir=$(printf '%s' "$input" | sed -n 's/.*"current_dir":"\([^"]*\)".*/\1/p' | head -1)
[ -n "$dir" ] || dir=$(printf '%s' "$input" | sed -n 's/.*"cwd":"\([^"]*\)".*/\1/p' | head -1)

# Display dir with $HOME shortened to ~.
case "$dir" in
	"$HOME"/*) disp="~${dir#"$HOME"}" ;;
	"$HOME")   disp="~" ;;
	*)         disp="$dir" ;;
esac
line="$disp"

# Append the git branch when inside a work tree.
if [ -n "$dir" ]; then
	branch=$(git -C "$dir" rev-parse --abbrev-ref HEAD 2>/dev/null)
	[ -n "$branch" ] && line="$line  $branch"
fi

# Orange write-mode segment with countdown; self-clears once the window lapses.
state="/tmp/forgejo-write-mode-${session}"
if [ -f "$state" ]; then
	expiry=$(cat "$state" 2>/dev/null)
	now=$(date +%s)
	if [ -n "$expiry" ] && [ "$now" -lt "$expiry" ]; then
		mins=$(( ( (expiry - now) + 59 ) / 60 ))
		line="$line   \033[1;38;5;208m\xe2\x97\x8f FORGEJO WRITE \xc2\xb7 ${mins}m\033[0m"
	else
		rm -f "$state"
	fi
fi

printf '%b\n' "$line"
