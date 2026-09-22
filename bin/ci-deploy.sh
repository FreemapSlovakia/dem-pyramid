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
# One deploy behind itself: ssh starts this file, and the reset below replaces
# it. Harmless -- git renames over the path, so the running shell keeps reading
# the inode it opened -- but a change to this script takes effect on the deploy
# after the one that ships it. Worth knowing before concluding a fix did not
# work.
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
# The last sha that was built, restarted and answered /health -- which is not
# the same as the checkout's previous HEAD. A re-run deploys what is already
# checked out, and a failed test run leaves the checkout moved while the old
# binary keeps serving; rolling back to HEAD would rebuild the bad commit in
# the first case and an untested one in the second. Outside the checkout, so
# reset and clean cannot take it.
LAST_GOOD="${LAST_GOOD:-/fm/storage2/dem/state/last-good-deploy}"

# Read by `dem-tool check` and by the service; a checkout of
# github.com/FreemapSlovakia/elevation-sources.
ELEVATION_SOURCES="${ELEVATION_SOURCES:-/fm/storage1/backend.freemap.sk-data/elevation-sources}"

# Under `command=` the argument arrives here instead of in "$@".
sha="${1:-${SSH_ORIGINAL_COMMAND:-}}"

# Whatever reaches this is attacker-controlled if the key ever leaks, and it is
# about to be handed to git. A full hex sha or nothing.
if [[ -n "$sha" && ! "$sha" =~ ^[0-9a-f]{40}$ ]]; then
	echo "ci-deploy: expected a 40-character commit sha, got '$sha'" >&2
	exit 64
fi

mkdir -p "$(dirname "$LAST_GOOD")"

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

# The pyramid's sources come from the elevation API's list and everything about
# them is measured from the rasters, so the only thing that can be wrong here is
# a cache that has fallen behind the data. `check` re-measures and fails if it
# has, rather than letting a deploy build against last month's geometry.
#
# Fetched first, and the working tree required clean: the list is authoritative
# from its repository, so a copy edited in place would check the cache against a
# local opinion rather than against what is published.
if [ -d "$ELEVATION_SOURCES/.git" ]; then
	git -C "$ELEVATION_SOURCES" fetch --quiet origin || {
		echo "ci-deploy: could not fetch the elevation source list" >&2
		exit 1
	}
	if [ -n "$(git -C "$ELEVATION_SOURCES" status --porcelain)" ]; then
		echo "ci-deploy: $ELEVATION_SOURCES has uncommitted changes -- commit and" >&2
		echo "           push them, or check them out again; it is a deployed" >&2
		echo "           artifact, not a scratch directory" >&2
		exit 1
	fi
	if [ "$(git -C "$ELEVATION_SOURCES" rev-parse HEAD)" \
	     != "$(git -C "$ELEVATION_SOURCES" rev-parse origin/main)" ]; then
		echo "ci-deploy: $ELEVATION_SOURCES is not at origin/main" >&2
		exit 1
	fi
else
	echo "ci-deploy: $ELEVATION_SOURCES is not a checkout -- clone" >&2
	echo "           https://github.com/FreemapSlovakia/elevation-sources there" >&2
	exit 1
fi

./target/release/dem-tool check

# Every step that must not be skipped says `|| return 1` rather than leaning on
# `set -e`: bash suppresses errexit inside a function called from an `&&` chain,
# so a failing `cp` here would otherwise be swallowed and the rollback would
# report success with the broken unit still installed.
install_unit() {
	sudo cmp -s deploy/terrain.service /etc/systemd/system/terrain.service && return 0
	# The unit carries TimeoutStopSec, which bounds the drain, so shipping the
	# binary without it deploys half the change: systemd's 90 s default kills a
	# render the drain was added to finish.
	echo "ci-deploy: unit changed, installing"
	sudo cp deploy/terrain.service /etc/systemd/system/terrain.service || return 1
	sudo systemctl daemon-reload || return 1
}

# The restart drains the render in flight, so it can sit for minutes; by the
# time it returns the new process is up or systemd has given up on it. Asking
# anyway, because a binary that starts and immediately fails leaves
# `Restart=on-failure` looping and the unit briefly looking fine.
healthy() {
	for _ in $(seq 30); do
		if curl --silent --fail --max-time 2 "$HEALTH" >/dev/null; then
			return 0
		fi
		sleep 1
	done
	return 1
}

rollback() {
	git reset --quiet --hard "$back" || return 1
	nice -n 19 "$CARGO" build --release --quiet || return 1
	# The unit too: the commit that just failed may be the one that installed a
	# broken one, and an old binary under it is not a rollback.
	install_unit || return 1
	sudo systemctl restart terrain || true
	healthy
}

install_unit || {
	echo "ci-deploy: could not install the unit" >&2
	exit 1
}

# Not left to `set -e`: a restart that fails is exactly when the rollback below
# is needed, and dying here would skip it silently. `healthy` decides instead.
sudo systemctl restart terrain || true

if healthy; then
	echo "ci-deploy: healthy at $(git rev-parse --short HEAD)"
	# Bookkeeping must not fail a deploy that worked, so a write that cannot
	# land is a warning. Via a temp file, because a half-written sha here is a
	# refused rollback later.
	if ! { printf '%s\n' "$target" >"$LAST_GOOD.tmp" && mv "$LAST_GOOD.tmp" "$LAST_GOOD"; }; then
		echo "ci-deploy: warning: could not record $target as known-good" >&2
	fi
	exit 0
fi

# It built and the tests passed, so this is a failure only the running service
# could show -- and nothing else will put the site back: the bad binary would
# keep looping under Restart=on-failure until someone rebuilt it by hand.
echo "ci-deploy: unhealthy after restart" >&2
systemctl --no-pager --lines=20 status terrain >&2 || true

# Held to what `$sha` is held to. It comes from a file rather than from the
# runner, but a garbled or garbage-collected sha here fails the reset, and a
# reset that fails mid-rollback would leave the checkout wherever it stopped.
back="$(cat "$LAST_GOOD" 2>/dev/null || true)"
if [[ -z "$back" || ! "$back" =~ ^[0-9a-f]{40}$ || "$back" == "$target" ]] ||
	! git merge-base --is-ancestor "$back" origin/main 2>/dev/null; then
	echo "ci-deploy: nothing known-good to roll back to; service is down at $target" >&2
	exit 1
fi

echo "ci-deploy: rolling back to $back" >&2
if rollback; then
	echo "ci-deploy: rolled back to $back, healthy" >&2
else
	echo "ci-deploy: rollback FAILED -- service is down; last known good was $back" >&2
fi
exit 1
