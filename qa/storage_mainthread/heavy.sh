#!/usr/bin/env bash
# Serialize builds and measurements with the other workspace's benchmarks.
set -u
for attempt in {1..8}; do
    printf '%s lock attempt %s/8\n' "$(date -u +%FT%TZ)" "$attempt" >&2
    flock -w 900 -E 75 /tmp/mistlib-heavy.lock "$@"
    status=$?
    if [ "$status" -ne 75 ]; then
        exit "$status"
    fi
done
printf 'Shared heavy-work lock unavailable after eight waits.\n' >&2
exit 75
