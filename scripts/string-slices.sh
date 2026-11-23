#!/usr/bin/env bash
# A string cut at a byte that is not a character's start ends the app. Clippy
# lists every raw slice; a file may not have more than string-slices.txt says.
# A new one whose offset came from that same text: run this with --update.
set -euo pipefail
export LC_ALL=C
cd "$(dirname "$0")/.."

known=scripts/string-slices.txt
out=$(mktemp)
trap 'rm -f "$out"' EXIT
cargo clippy --bin ideae --message-format=short -- -A clippy::all -W clippy::string_slice >"$out" 2>&1 || { cat "$out" >&2; exit 1; }
now=$(grep -E '^src/.*indexing into a string' "$out" | cut -d: -f1 | sort | uniq -c | awk '{print $2, $1}')

if [ "${1:-}" = --update ]; then
  printf '%s\n' "$now" >"$known"
  exit 0
fi

over=$(join -a1 -e0 -o 0,1.2,2.2 <(printf '%s\n' "$now") "$known" | awk '$2 > $3 {print "  " $1 ": " $2 " raw string slices, " $3 " known"}')
if [ -n "$over" ]; then
  echo "A string is cut at a byte offset:" >&2
  echo "$over" >&2
  grep -E '^src/.*indexing into a string' "$out" | cut -d: -f1-2 | sed 's/^/    /' >&2
  echo 'Use chars().take(n), str::get or is_char_boundary. If the offset came from that same text: scripts/string-slices.sh --update' >&2
  exit 1
fi
