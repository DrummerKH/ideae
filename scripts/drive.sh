#!/bin/bash
# Runs the debug build on a private Xvfb, plays input steps, and stops it.
#
#   scripts/drive.sh LOG STARTUP_SECS "steps..." -- [app args...]
#
# Steps (scripts/xdrive.py): move:X,Y click rclick press release wheel:up|down
# down:ctrl up:ctrl type:text key:return wait:SECS shot:FILE.png
#
# The app gets no D-Bus session, so it cannot touch the real desktop (dash
# favorites, notifications). Point IDEAE_STATE at a scratch file to keep the
# real state out of it.
set -u
log=$1; startup=$2; steps=$3; shift 3; [ "${1:-}" = "--" ] && shift
here=$(cd "$(dirname "$0")" && pwd)
display=:${IDEAE_DISPLAY:-77}
Xvfb "$display" -screen 0 1600x1000x24 >/dev/null 2>&1 & xpid=$!
sleep 1
env -u WAYLAND_DISPLAY DBUS_SESSION_BUS_ADDRESS=disabled: DISPLAY="$display" \
  "$here/../target/debug/ideae" "$@" > "$log" 2>&1 & apid=$!
sleep "$startup"
DISPLAY="$display" python3 -I "$here/xdrive.py" $steps
# SIGTERM quits the app the way Exit does; it gets a moment to clean up.
kill $apid 2>/dev/null; sleep 2; kill -9 $apid 2>/dev/null
kill $xpid; wait 2>/dev/null
