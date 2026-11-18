#!/usr/bin/env bash
# Before a push: the tests, the end-to-end ones (real repositories, real
# terminals, a real daemon; under a minute together), and the Windows side.
# Edit prediction's runs a real model only when IDEAE_PREDICT_SERVER and
# IDEAE_PREDICT_MODEL name one.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build
cargo test
cargo test -- --ignored \
  an_acp_agent \
  an_open_file \
  two_variants \
  two_agents \
  the_tasks_of_the_last_run \
  a_window_that_does_not_read \
  a_task_is_set_up_checked_and_torn_down \
  an_archived_task \
  a_page_tells_the_tab \
  a_task_s_browser_wakes_when_called \
  a_local_model_predicts_at_the_cursor \
  a_local_model_predicts_the_next_edits

if rustup target list --installed 2>/dev/null | grep -q x86_64-pc-windows-gnu; then
  cargo check --target x86_64-pc-windows-gnu --target-dir target/win
else
  echo "no x86_64-pc-windows-gnu target: the Windows side is not checked" >&2
fi
