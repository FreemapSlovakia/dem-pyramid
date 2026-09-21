#!/usr/bin/env bash
# Build and restart the terrain service on fm6. Run *on fm6*, by the CI key.
#
#   bin/ci-deploy.sh <sha>     deploy that commit
#   bin/ci-deploy.sh           deploy whatever origin/main points at
#
# The counterpart to sync.sh, which pushes an unreviewed working tree by hand.
# This one only ever deploys a commit that is already on origin/main, so what
# is running can be named: `git -C /fm/storage2/dem/build rev-parse HEAD`.
#
# Built here rather than on the runner because the binary links fm6's libgdal
# (3.10.3) and a runner would build against whatever Ubuntu ships. glibc is not
# the reason -- fm6's 2.41 is newer than a runner's -- but the soname is.
#
# The nginx side is not deployed here: bin/deploy-nginx.sh writes under
# /etc/nginx and reloads nginx, which is a different blast radius and a
# different review. The systemd unit is installed below, because the drain
# added with it is only as long as TimeoutStopSec says.
#
# ## One-time setup
#
# Two repository secrets, which .github/workflows/deploy.yml reads:
#
#   FM6_DEPLOY_KEY    the private half of the key pinned below
#   FM6_KNOWN_HOSTS   fm6's host key, in the bracketed form a non-default port
#                     takes: `[fm6.freemap.sk]:21122 ssh-ed25519 AAAA...`.
#                     `ssh-keyscan fm6.freemap.sk` does not produce that form,
#                     and the plain one fails every deploy at host-key
#                     verification with nothing here to explain why. Take it
#                     from `ssh-keyscan -p 21122 fm6.freemap.sk`.
#
# The build directory is an rsync target today and has to become a checkout,
# which sync.sh keeps working with (it excludes .git, and rsync does not delete
# what it excludes):
#
#   cd /fm/storage2/dem/build
#   git init -b main && git remote add origin \
#       https://github.com/FreemapSlovakia/dem-pyramid.git
#   git fetch origin main && git reset --hard origin/main
#
# Then pin the CI key to this script, so a leaked key cannot ask fm6 for
# anything else -- worth doing carefully, because this login has passwordless
# sudo. In ~/.ssh/authorized_keys, on one line:
#
#   command="/fm/storage2/dem/build/bin/ci-deploy.sh",no-agent-forwarding,\
#   no-port-forwarding,no-pty,no-user-rc,no-X11-forwarding ssh-ed25519 AAAA... ci
#
# `command=` overrides whatever the runner asks for and leaves the request in
# SSH_ORIGINAL_COMMAND, which is where the sha below comes from -- and why it
# is validated rather than trusted.

set -euo pipefail

BUILD_DIR="${BUILD_DIR:-/fm/storage2/dem/build}"
HEALTH="${HEALTH:-http://127.0.0.1:3100/health}"
# Spelled out because a forced command gets a minimal PATH, as in sync.sh.
CARGO="${CARGO:-$HOME/.cargo/bin/cargo}"

# Under `command=` the argument arrives here instead of in "$@".
sha="${1:-${SSH_ORIGINAL_COMMAND:-}}"

# Whatever reaches this is attacker-controlled if the key ever leaks, and it is
# about to be handed to git. A full hex sha or nothing.
if [[ -n "$sha" && ! "$sha" =~ ^[0-9a-f]{40}$ ]]; then
	echo "ci-deploy: expected a 40-character commit sha, got '$sha'" >&2
	exit 64
fi

cd "$BUILD_DIR"
git rev-parse --git-dir >/dev/null 2>&1 || {
	echo "ci-deploy: $BUILD_DIR is not a checkout -- see the setup in this script" >&2
	exit 1
}

git fetch --quiet origin main
target="${sha:-$(git rev-parse origin/main)}"

# Only what is on main. Without this the key could deploy any commit in the
# repository, including one that only ever existed on a fork's branch.
if ! git merge-base --is-ancestor "$target" origin/main; then
	echo "ci-deploy: $target is not on origin/main" >&2
	exit 65
fi

was="$(git rev-parse HEAD)"
git reset --quiet --hard "$target"
git clean --quiet -fd # not -x: target/ is ignored, and rebuilding it costs minutes
echo "ci-deploy: $was -> $target"

# Nice: the box serves tiles and the API too, and a release build takes all
# twelve cores otherwise. Only the binary is replaced, and only on success, so
# a build that fails leaves the running service untouched.
nice -n 19 "$CARGO" build --release --quiet

# Run here rather than on the runner because they link the same libgdal the
# binary will. They gate the restart, so a red build leaves the old service
# running -- with the checkout already moved, which `git rev-parse HEAD` will
# then disagree with the running binary about until the next deploy.
nice -n 19 "$CARGO" test --release --quiet

# The unit carries TimeoutStopSec, which bounds the drain, so shipping the
# binary without it deploys half the change: systemd's 90 s default kills a
# render the drain was added to finish.
if ! sudo cmp -s deploy/terrain.service /etc/systemd/system/terrain.service; then
	echo "ci-deploy: unit changed, installing"
	sudo cp deploy/terrain.service /etc/systemd/system/terrain.service
	sudo systemctl daemon-reload
fi

sudo systemctl restart terrain

# The restart drains in-flight renders, so it can sit for minutes; by the time
# it returns the new process is up or systemd has given up on it. Health is
# still worth asking about, because a binary that starts and immediately fails
# leaves `Restart=on-failure` looping and the unit briefly looking fine.
for _ in $(seq 30); do
	if curl --silent --fail --max-time 2 "$HEALTH" >/dev/null; then
		echo "ci-deploy: healthy at $(git rev-parse --short HEAD)"
		exit 0
	fi
	sleep 1
done

# It built and the tests passed, so this is a failure only the running service
# could show. Nothing else will put the site back: without this the bad binary
# keeps looping under Restart=on-failure until someone rebuilds by hand.
echo "ci-deploy: unhealthy after restart; rolling back to $was" >&2
systemctl --no-pager --lines=20 status terrain >&2 || true
git reset --quiet --hard "$was"
if nice -n 19 "$CARGO" build --release --quiet && sudo systemctl restart terrain; then
	echo "ci-deploy: rolled back to $was" >&2
else
	echo "ci-deploy: rollback FAILED -- service is down at $target" >&2
fi
exit 1
