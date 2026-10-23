#!/bin/sh
# Backs up the journal — which is the whole platform.
#
# There is no database to dump. Every balance, position and decision is a
# fold over the event log in the journal volume, so copying that directory
# copies the business. Losing it loses who owns what money, and no amount of
# application code recovers from that.
#
# What this does:
#   * takes a consistent copy of the journal volume into a dated tarball
#   * keeps the last KEEP of them
#   * verifies the tarball before deleting anything older
#
# What it does NOT do, and you must:
#   * copy the tarballs OFF this machine. A backup on the same disk as the
#     thing it backs up is not a backup.
#   * restore one, on purpose, before you need to. See ops/restore.sh. An
#     untested restore is a hope, not a plan.
#
# Run it from cron, e.g. hourly:
#   0 * * * * /opt/feraro/ops/backup.sh >> /var/log/feraro-backup.log 2>&1

set -eu

PROJECT="${FERARO_PROJECT:-feraro}"
VOLUME="${FERARO_VOLUME:-${PROJECT}_journal}"
OUT="${FERARO_BACKUP_DIR:-/var/backups/feraro}"
KEEP="${FERARO_BACKUP_KEEP:-48}"

stamp=$(date -u +%Y%m%dT%H%M%SZ)
archive="$OUT/journal-$stamp.tar.gz"

mkdir -p "$OUT"

# The journal is append-only and fsynced on a timer, so a copy taken while
# the gateway runs is a copy of a prefix of the log — which replays. That is
# the property that makes this safe without stopping the platform: a torn
# tail is indistinguishable from a crash, and the engine already refuses a
# corrupt segment rather than serving a wrong balance from it.
docker run --rm \
	-v "$VOLUME":/journal:ro \
	-v "$OUT":/out \
	busybox:1.37-musl \
	tar czf "/out/journal-$stamp.tar.gz" -C /journal .

# A tarball nobody can open is worse than no tarball, because it is counted.
if ! tar tzf "$archive" >/dev/null 2>&1; then
	echo "backup: $archive did not verify; keeping everything and failing" >&2
	rm -f "$archive"
	exit 1
fi

size=$(wc -c <"$archive")
echo "backup: $archive ($size bytes) verified"

# Prune only after a good one exists.
count=$(find "$OUT" -maxdepth 1 -name 'journal-*.tar.gz' | wc -l)
if [ "$count" -gt "$KEEP" ]; then
	find "$OUT" -maxdepth 1 -name 'journal-*.tar.gz' \
		| sort \
		| head -n "$((count - KEEP))" \
		| while IFS= read -r old; do
			echo "backup: pruning $old"
			rm -f "$old"
		done
fi

echo "backup: $(find "$OUT" -maxdepth 1 -name 'journal-*.tar.gz' | wc -l) kept in $OUT"
echo "backup: REMEMBER — copy these off this machine."
