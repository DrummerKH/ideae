#!/usr/bin/env bash
# Before a push: the tests, the end-to-end ones (real repositories, real
# terminals, a real daemon; under a minute together), and the Windows side.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo test
cargo test -- --ignored \
  an_open_file \
  two_variants \
  two_agents \
  the_tasks_of_the_last_run \
  a_window_that_does_not_read \
  a_task_is_set_up_checked_and_torn_down \
  an_archived_task

if rustup target list --installed 2>/dev/null | grep -q x86_64-pc-windows-gnu; then
  cargo check --target x86_64-pc-windows-gnu --target-dir target/win
else
  echo "no x86_64-pc-windows-gnu target: the Windows side is not checked" >&2
fi
