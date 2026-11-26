#!/usr/bin/env bash
# Projects on another machine against real sshd: two throwaway containers
# (Docker), one reached directly and through the other, which reaches it
# with its own key; and a password asked once. Removes all it made.
#
#   scripts/remote-live.sh
#
# IDEAE_LIVE_PLATFORM=linux/amd64 runs the second container for another
# architecture (where Docker emulates it), so both of ideae-server's Linux
# builds are tried.
set -euo pipefail
cd "$(dirname "$0")/.."

command -v docker >/dev/null || { echo "remote-live: needs docker" >&2; exit 1; }
scripts/server.sh >/dev/null
cargo build -q

tag="ideae-live-$$"
work=$(mktemp -d)
cleanup() {
  docker rm -f "$tag-hub" "$tag-beyond" >/dev/null 2>&1 || true
  docker network rm "$tag" >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT

ssh-keygen -q -t ed25519 -N '' -f "$work/key"
password=$(head -c 12 /dev/urandom | od -An -tx1 | tr -d ' \n')
# The image is kept for the next run; the key and the password are this run's, given to its containers.
mkdir "$work/image"
cat > "$work/image/Dockerfile" <<'EOF'
FROM alpine:3.20
RUN apk add --no-cache openssh bash && ssh-keygen -A \
 && sed -i 's/^AllowTcpForwarding no/AllowTcpForwarding yes/' /etc/ssh/sshd_config \
 && adduser -D -s /bin/bash dev && adduser -D other \
 && mkdir -p /home/dev/.ssh && echo 'echo "Welcome to the test box"' > /home/dev/.bashrc \
 && chown -R dev:dev /home/dev && chmod 700 /home/dev/.ssh
CMD ["/usr/sbin/sshd", "-D", "-e"]
EOF
image() { echo "ideae-live-sshbox${1:+-${1//\//-}}"; }
build() { docker build -q ${1:+--platform "$1"} -t "$(image "$1")" "$work/image" >/dev/null; }
build ""
[ -n "${IDEAE_LIVE_PLATFORM:-}" ] && build "$IDEAE_LIVE_PLATFORM"

docker network create "$tag" >/dev/null
docker run -d --name "$tag-hub" --network "$tag" -p 127.0.0.1::22 "$(image "")" >/dev/null
docker run -d --name "$tag-beyond" --network "$tag" -p 127.0.0.1::22 ${IDEAE_LIVE_PLATFORM:+--platform "$IDEAE_LIVE_PLATFORM"} "$(image "${IDEAE_LIVE_PLATFORM:-}")" >/dev/null
for c in "$tag-hub" "$tag-beyond"; do
  docker cp "$work/key.pub" "$c:/home/dev/.ssh/authorized_keys"
  docker exec "$c" sh -c "chown dev /home/dev/.ssh/authorized_keys && chmod 600 /home/dev/.ssh/authorized_keys && echo 'dev:$password' | chpasswd"
done
port() { docker port "$1" 22/tcp | head -1 | sed 's/.*://'; }
hub_port=$(port "$tag-hub")
beyond_port=$(port "$tag-beyond")

# The hub reaches the other with a key of its own, and has met it once.
docker cp "$work/key" "$tag-hub:/home/dev/.ssh/id_ed25519"
docker exec "$tag-hub" sh -c 'chown dev /home/dev/.ssh/id_ed25519 && chmod 600 /home/dev/.ssh/id_ed25519'
for _ in $(seq 50); do
  docker exec -u dev "$tag-hub" ssh -o StrictHostKeyChecking=accept-new -o BatchMode=yes "dev@$tag-beyond" true 2>/dev/null && break
  sleep 0.2
done

ssh="ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -o IdentitiesOnly=yes"
fails=0
run() {
  local name=$1; shift
  echo "== $name"
  if env "$@" cargo test -q -- --ignored --test-threads=1 a_real_server_gets a_real_server_asks 2>&1 | tee "$work/out" | grep -E -A2 "panicked|^test result"; then :; fi
  grep -q "^test result: ok. 2 passed" "$work/out" || { echo "remote-live: $name failed" >&2; fails=$((fails + 1)); }
}
# IDEAE_LIVE_OTHER: a command run there as another user of that machine (a port of yours is not theirs).
run "direct, $(docker exec "$tag-hub" uname -m)" IDEAE_LIVE_REMOTE="$ssh -i $work/key -p $hub_port dev@127.0.0.1" \
  IDEAE_LIVE_PASSWORD_REMOTE="$ssh -o PubkeyAuthentication=no -p $hub_port dev@127.0.0.1" IDEAE_LIVE_PASSWORD="$password" \
  IDEAE_LIVE_OTHER="docker exec -u other $tag-hub"
run "direct, $(docker exec "$tag-beyond" uname -m)" IDEAE_LIVE_REMOTE="$ssh -i $work/key -p $beyond_port dev@127.0.0.1" IDEAE_LIVE_OTHER="docker exec -u other $tag-beyond"
run "through the hub" IDEAE_LIVE_REMOTE="$ssh -i $work/key -p $hub_port dev@127.0.0.1" IDEAE_LIVE_HOP="dev@$tag-beyond" IDEAE_LIVE_OTHER="docker exec -u other $tag-beyond"

# Nothing of a run is left there but the daemon, which an Exit would end.
for c in "$tag-hub" "$tag-beyond"; do
  left=$(docker exec "$c" ps -o args | grep -v -e '^COMMAND' -e sshd -e 'ps -o' -e '--daemon' || true)
  [ -z "$left" ] || { echo "remote-live: left running on $c:" >&2; echo "$left" >&2; fails=$((fails + 1)); }
done
[ "$fails" = 0 ] && echo "remote-live: all passed"
exit "$fails"
