#!/bin/sh
# Puts a backed-up journal back, and proves it replays.
#
# Run this on purpose, on a spare machine, before you ever need it. A backup
# nobody has restored is a file of unknown value; the first time you find out
# must not be the morning the disk died.
#
#   ops/restore.sh /var/backups/feraro/journal-20261023T090000Z.tar.gz
#
# It refuses to overwrite a journal that already has anything in it. If you
# genuinely mean to replace a live one, move it aside yourself — this script
# will not be the reason a platform forgot who owned what.

set -eu

archive="${1:-}"
PROJECT="${FERARO_PROJECT:-feraro}"
VOLUME="${FERARO_VOLUME:-${PROJECT}_journal}"

if [ -z "$archive" ]; then
	echo "usage: ops/restore.sh <journal-....tar.gz>" >&2
	exit 2
fi
if [ ! -f "$archive" ]; then
	echo "restore: no such archive: $archive" >&2
	exit 2
fi
if ! tar tzf "$archive" >/dev/null 2>&1; then
	echo "restore: $archive does not open; it is not a usable backup" >&2
	exit 1
fi

existing=$(docker run --rm -v "$VOLUME":/journal busybox:1.37-musl \
	sh -c 'ls -1 /journal 2>/dev/null | wc -l' || echo 0)
if [ "$existing" -gt 0 ]; then
	echo "restore: volume $VOLUME already holds $existing entries." >&2
	echo "restore: refusing to overwrite. Move it aside first if you mean this." >&2
	exit 1
fi

dir=$(cd "$(dirname "$archive")" && pwd)
file=$(basename "$archive")

docker run --rm \
	-v "$VOLUME":/journal \
	-v "$dir":/in:ro \
	busybox:1.37-musl \
	tar xzf "/in/$file" -C /journal

echo "restore: unpacked into $VOLUME"

# Replaying is the test. The engine refuses a corrupt or gappy journal rather
# than serving a wrong balance from it, so a gateway that starts and reports
# healthy has proved the backup — which is the only proof worth having.
echo "restore: starting the gateway to replay it..."
docker compose -p "$PROJECT" up -d gateway

i=0
while [ "$i" -lt 30 ]; do
	if docker compose -p "$PROJECT" exec -T gateway /usr/local/bin/gateway --healthcheck >/dev/null 2>&1; then
		echo "restore: the gateway replayed the journal and is healthy."
		exit 0
	fi
	i=$((i + 1))
	sleep 2
done

echo "restore: the gateway did not become healthy. Read its log:" >&2
echo "  docker compose -p $PROJECT logs gateway" >&2
exit 1
