#!/usr/bin/env bash
# Real network loss simulation for the Constellation FEC encoder.
#
# Runs sender/receiver as persistent Docker containers on a user-defined
# bridge network, warms up ARP resolution between them *before* any loss is
# introduced, then applies `tc netem loss X%` on the SENDER's interface via
# NET_ADMIN and sweeps loss percentages straddling the theoretical 75%
# boundary (DATA_PSHREDS/TOTAL_PSHREDS = 64/256 = 25% needed => up to 75%
# loss tolerable, i.e. losing up to 192 of 256 pshreds).
#
# netem shapes *egress* traffic on the interface it's applied to. Pshreds
# travel sender -> receiver, so the loss rule must go on the sender's
# interface -- an earlier version of this script applied it to the
# receiver's interface instead, which only affected the receiver's own
# egress (ARP replies, and otherwise nothing, since the receiver doesn't
# send data back), so the pshred stream itself was never actually shaped.
#
# Containers are kept alive for the whole sweep (rather than recreated per
# iteration) specifically so ARP/neighbor state stays warm: netem loss
# applied to an interface affects ALL egress traffic on it, including ARP
# requests/replies, so if containers (and therefore MAC addresses) were
# recreated per-iteration, high loss percentages would make ARP resolution
# itself fail and produce a false "0 pshreds received" cliff unrelated to
# the FEC recovery threshold being tested.
#
# Runs on the Docker host (macOS in this project, since tc/iproute2 don't
# exist natively there) via `docker compose`; this is a local dev tool, not
# wired into CI. A single UDP run at netem's probabilistic loss rate is not a
# strict pass/fail boundary the way the in-process 63-of-256 unit test is --
# treat results near 75% as a sanity check on realistic conditions, not a
# bit-exact guarantee.
set -euo pipefail

cd "$(dirname "$0")"

LOSS_PERCENTAGES=(0 25 50 70 74 75 76 80 90)
PAYLOAD_BYTES="${PAYLOAD_BYTES:-65536}"
export PAYLOAD_BYTES

compose() {
    docker compose -f compose.yaml "$@"
}

cleanup() {
    compose down -v --remove-orphans >/dev/null 2>&1 || true
}
trap cleanup EXIT

echo "Building images..."
compose build

echo "Starting persistent sender/receiver containers..."
compose up -d

echo "Warming up ARP resolution before introducing any loss..."
compose exec -T sender ping -c 2 -W 1 10.88.0.3 >/dev/null
compose exec -T receiver ping -c 2 -W 1 10.88.0.2 >/dev/null

results=()

for loss in "${LOSS_PERCENTAGES[@]}"; do
    echo
    echo "=== loss ${loss}% ==="

    if [[ "$loss" == "0" ]]; then
        compose exec -T sender tc qdisc del dev eth0 root >/dev/null 2>&1 || true
    else
        compose exec -T sender tc qdisc replace dev eth0 root netem loss "${loss}%"
    fi

    compose exec -T receiver ./target/release/receiver 0.0.0.0:9000 "$PAYLOAD_BYTES" &
    recv_pid=$!
    sleep 0.3 # let the receiver bind before the sender starts firing

    compose exec -T sender ./target/release/sender 10.88.0.3:9000 "$PAYLOAD_BYTES"

    set +e
    wait "$recv_pid"
    exit_code=$?
    set -e

    if [[ "$exit_code" == "0" ]]; then
        echo "loss ${loss}%: PASS"
        results+=("${loss}%: PASS")
    else
        echo "loss ${loss}%: FAIL (exit code $exit_code)"
        results+=("${loss}%: FAIL")
    fi
done

compose exec -T sender tc qdisc del dev eth0 root >/dev/null 2>&1 || true

echo
echo "=== summary (payload=${PAYLOAD_BYTES} bytes, tolerable loss <= 75%) ==="
for r in "${results[@]}"; do
    echo "  $r"
done
