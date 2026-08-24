#!/usr/bin/env bash
# Install the nginx side of the terrain service on fm6.
#
#   deploy/terrain.freemap.sk.conf -> /etc/nginx/sites-available/terrain.freemap.sk
#   deploy/terrain-limits.conf     -> /etc/nginx/conf.d/
#   deploy/terrain-cors.conf       -> /etc/nginx/snippets/
#
#   bin/deploy-nginx.sh            install and reload
#   bin/deploy-nginx.sh --check    diff only, change nothing
#
# The counterpart to sync.sh, which deploys the service itself. Split because
# they need different rights: sync.sh runs entirely as the login user, this
# writes under /etc and needs a sudo password, so it cannot run unattended.
#
# Worth having as a script rather than three cp lines in a comment because the
# CORS snippet arrived as a fourth file and the comment did not mention it --
# which is exactly how a file gets edited in the repo, never copied, and
# quietly disagrees with the host for a month.

set -euo pipefail

HOST="${HOST:-fm6}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# local file : path on the host
FILES=(
	"deploy/terrain.freemap.sk.conf:/etc/nginx/sites-available/terrain.freemap.sk"
	"deploy/terrain-limits.conf:/etc/nginx/conf.d/terrain-limits.conf"
	"deploy/terrain-cors.conf:/etc/nginx/snippets/terrain-cors.conf"
)

if [[ "${1:-}" == "--check" ]]; then
	drift=0
	for pair in "${FILES[@]}"; do
		src="$HERE/${pair%%:*}"
		dst="${pair##*:}"
		# `cat` over ssh rather than scp: a file the host is missing
		# entirely should read as drift, not as a transfer error.
		if ssh "$HOST" "cat $dst" 2>/dev/null | diff -q - "$src" >/dev/null 2>&1; then
			echo "in sync   ${pair%%:*}"
		else
			echo "DIFFERS   ${pair%%:*}"
			drift=1
		fi
	done
	exit $drift
fi

# Staged in /tmp first, then moved with one sudo. Copying straight into /etc
# would need the whole transfer to run as root.
stage=$(ssh "$HOST" "mktemp -d")
# shellcheck disable=SC2064  # expand $stage now, not when the trap fires
trap "ssh '$HOST' 'rm -rf $stage'" EXIT

for pair in "${FILES[@]}"; do
	scp -q "$HERE/${pair%%:*}" "$HOST:$stage/$(basename "${pair##*:}")"
done

# One ssh -t so a single sudo prompt covers the lot, and `nginx -t` before the
# reload so a syntax error leaves the running config alone rather than taking
# the vhost down. No backup copy: the previous version is in git, and stale
# `.bak` files in sites-available outlive any memory of why they are there.
ssh -t "$HOST" "
	set -euo pipefail
	sudo cp $stage/terrain.freemap.sk /etc/nginx/sites-available/terrain.freemap.sk
	sudo cp $stage/terrain-limits.conf /etc/nginx/conf.d/
	sudo cp $stage/terrain-cors.conf /etc/nginx/snippets/
	sudo nginx -t
	sudo systemctl reload nginx
"

echo "nginx config deployed -> $HOST"
