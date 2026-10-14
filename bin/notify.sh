#!/usr/bin/env bash
# Mail how the weekly peaks update went, as the Wikimedia Commons import does:
# through Mailgun, with the settings in /etc/freemap.conf, which only
# peaks-update-notify@.service reads.
#
# Usage: bin/notify.sh succeeded|failed

set -euo pipefail

DEM_ROOT="${DEM_ROOT:-/fm/storage2/dem}"
outcome="$1"

case "${MAILGUN_ENABLE:-}" in
1 | true | yes) ;;
*)
  echo "notify: MAILGUN_ENABLE is off, not mailing" >&2
  exit 0
  ;;
esac
to="${PEAKS_UPDATE_NOTIFY_EMAIL:-${WIKIMEDIA_IMPORT_NOTIFY_EMAIL:-}}"
if [ -z "$to" ]; then
  echo "notify: no PEAKS_UPDATE_NOTIFY_EMAIL, not mailing" >&2
  exit 0
fi

# A step's lines from its latest run in its log.
latest() {
  awk -v s="=== $1 start" 'index($0, s) == 1 { buf = "" } { buf = buf $0 "\n" } END { printf "%s", buf }' \
    "$DEM_ROOT/logs/$1.log" 2>/dev/null || true
}

if [ "$outcome" = succeeded ]; then
  subject="Peaks update succeeded on $(hostname)"
  body="$(latest peaks | grep -E '^peaks: ' || true)
$(latest prominence | grep -E ' peaks: | saddles: |^prominence: ' || true)"
else
  subject="Peaks update FAILED on $(hostname)"
  # run.sh writes a step's status when it ends; a missing one never ended.
  step=prominence
  [ "$(cat "$DEM_ROOT/state/peaks.status" 2>/dev/null)" = 0 ] || step=peaks
  body="Step $step failed; peaks.gpkg is left as it was.

$(latest "$step" | tail -n 40)"
fi
body="$body

Logs: $DEM_ROOT/logs/peaks.log, $DEM_ROOT/logs/prominence.log
      journalctl -u peaks-update.service"

eu=""
case "${MAILGUN_EU:-}" in 1 | true | yes) eu=.eu ;; esac
curl --silent --show-error --fail --max-time 60 --user "api:$MAILGUN_API_KEY" \
  "https://api$eu.mailgun.net/v3/$MAILGUN_DOMAIN/messages" \
  --form-string "from=Freemap <noreply@freemap.sk>" \
  --form-string "to=$to" \
  --form-string "subject=$subject" \
  --form-string "text=$body" >/dev/null
