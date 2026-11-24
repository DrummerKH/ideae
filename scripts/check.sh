#!/usr/bin/env bash
# Before a push: the tests, the end-to-end ones (real repositories, real
# terminals, a real daemon; under a minute together), and the Windows side.
# Edit prediction's runs a real model only when IDEAE_PREDICT_SERVER and
# IDEAE_PREDICT_MODEL name one.
set -euo pipefail
cd "$(dirname "$0")/.."

# The browser's two tests need a Chrome, and pass without running where
# there is none: said here, where a passing test's own words are not shown.
chrome="${IDEAE_CHROME:-}"
for app in "Google Chrome" Chromium "Microsoft Edge"; do
  [ -d "/Applications/$app.app" ] && chrome=$app
done
for program in google-chrome google-chrome-stable chromium chromium-browser; do
  command -v "$program" >/dev/null 2>&1 && chrome=$program
done
[ -n "$chrome" ] || echo "no Chrome or Chromium here: the browser's tests are skipped (name one in IDEAE_CHROME)" >&2

cargo build
scripts/string-slices.sh
cargo test
cargo test -- --ignored \
  an_acp_agent \
  a_sql_file_runs \
  an_open_file \
  two_variants \
  two_agents \
  a_question_beside \
  agents_start_agents \
  a_ticket_task_is_named \
  the_tasks_of_the_last_run \
  a_window_that_does_not_read \
  a_task_is_set_up_checked_and_torn_down \
  an_archived_task \
  a_page_tells_the_tab \
  a_task_s_browser_wakes_when_called \
  a_stream_has_the_screen_s_pixels \
  a_local_model_predicts_at_the_cursor \
  a_local_model_predicts_the_next_edits \
  two_tasks_share_one_rust_analyzer \
  an_agent_asks_the_language_server

if rustup target list --installed 2>/dev/null | grep -q x86_64-pc-windows-gnu; then
  # With the tests: CI builds them there too, and code only they have was once left for Unix alone.
  cargo check --tests --target x86_64-pc-windows-gnu --target-dir target/win
else
  echo "no x86_64-pc-windows-gnu target: the Windows side is not checked" >&2
fi
