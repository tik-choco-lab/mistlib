#!/usr/bin/env bash
set -euo pipefail

# Loopback UDP is WebRTC; keep TCP (relay, HTTP, harness) on the unshaped band.
# Both directions share this band; limit includes packets held in the delay stage.
# Usage: netem.sh on [delay-ms [rate-kbit [limit-packets [loss random P% | loss gemodel P% R% H% K%]]]] | netem.sh off
case "${1:-}" in
  on)
    [[ $# -le 10 ]]
    delay_ms=${2:-150}
    rate_kbit=${3:-1000}
    limit_args=()
    if [[ $# -ge 4 ]]; then
      [[ "$4" =~ ^[1-9][0-9]*$ ]]
      limit_args=(limit "$4")
    fi
    loss_args=()
    if [[ $# -gt 4 ]]; then
      [[ "$5" == loss ]]
      case "$6" in
        random) [[ $# -eq 7 ]] ;;
        gemodel) [[ $# -eq 10 ]] ;;
        *) exit 2 ;;
      esac
      for probability in "${@:7}"; do
        [[ "$probability" =~ ^([0-9]+)(\.[0-9]+)?%$ ]]
        awk -v p="${probability%\%}" 'BEGIN { exit !(p >= 0 && p <= 100) }'
      done
      loss_args=("${@:5}")
    fi
    [[ "$delay_ms" =~ ^[1-9][0-9]*$ && "$rate_kbit" =~ ^[1-9][0-9]*$ ]]
    # Only remove a root that this invocation successfully installed. In
    # particular, a failed `add` must never delete somebody else's qdisc.
    tc qdisc add dev lo root handle 1: prio bands 3 priomap 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0
    trap 'tc qdisc del dev lo root 2>/dev/null || true' ERR
    trap 'tc qdisc del dev lo root 2>/dev/null || true; exit 130' INT
    trap 'tc qdisc del dev lo root 2>/dev/null || true; exit 143' TERM
    trap 'tc qdisc del dev lo root 2>/dev/null || true; exit 129' HUP
    tc qdisc add dev lo parent 1:3 handle 30: netem delay "${delay_ms}ms" rate "${rate_kbit}kbit" "${limit_args[@]}" "${loss_args[@]}"
    tc filter add dev lo parent 1: protocol ip pref 1 u32 match ip protocol 17 0xff flowid 1:3
    tc filter add dev lo parent 1: protocol ipv6 pref 2 u32 match ip6 protocol 17 0xff flowid 1:3
    ;;
  off)
    tc qdisc del dev lo root 2>/dev/null || true
    ;;
  *)
    echo 'Usage: netem.sh on [delay-ms [rate-kbit [limit-packets]]] | netem.sh off' >&2
    exit 2
    ;;
esac
