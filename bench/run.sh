#!/usr/bin/env bash
# Builds the C original and the Rust builds from their git refs, runs the
# benchmark matrix in bench.py against each, and writes results.md.
#
#   bench/run.sh                 # everything, into target/bench
#   OUT=/tmp/b CLIENTS=64 bench/run.sh
#
# Refs can be overridden with C_REF, PORT_REF, MODERN_REF and SMOOTH_REF.
set -euo pipefail

REPO=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
BENCH="$REPO/bench/bench.py"
OUT=${OUT:-$REPO/target/bench}
CLIENTS=${CLIENTS:-16}
DURATION=${DURATION:-20}
REPEATS=${REPEATS:-3}
C_REF=${C_REF:-master}
PORT_REF=${PORT_REF:-rust-port-and-tests}
MODERN_REF=${MODERN_REF:-rust-modernization}
SMOOTH_REF=${SMOOTH_REF:-rust-smoothing}

mkdir -p "$OUT/bin" "$OUT/src"

# export_ref NAME REF: a clean copy of REF's tree under $OUT/src/NAME.
export_ref() {
	rm -rf "$OUT/src/$1"
	mkdir -p "$OUT/src/$1"
	git -C "$REPO" archive "$2" | tar -x -C "$OUT/src/$1"
	git -C "$REPO" rev-parse --short "$2" > "$OUT/src/$1.rev"
}

build_c() {
	echo "== building C original from $C_REF"
	export_ref c "$C_REF"
	# The C sources predate C23, where true and false became keywords.
	cmake -S "$OUT/src/c" -B "$OUT/src/c/build" -DCMAKE_BUILD_TYPE=Release \
		-DCMAKE_C_FLAGS=-std=gnu11 -Wno-dev > /dev/null
	cmake --build "$OUT/src/c/build" --parallel > /dev/null
	cp "$OUT/src/c/build/qwfwd" "$OUT/bin/c"
}

# build_rust NAME REF: a release build in its own target directory. (A shared
# one is not safe: trees exported from commits made in the same second get
# identical mtimes and cargo may consider the second build up to date.)
build_rust() {
	echo "== building $1 from $2"
	export_ref "$1" "$2"
	(cd "$OUT/src/$1" && CARGO_TARGET_DIR="$OUT/cargo/$1" cargo build --release --quiet)
	cp "$OUT/cargo/$1/release/qwfwd" "$OUT/bin/$1"
}

# bench NAME [bench.py options...]: REPEATS runs; the report takes medians.
bench() {
	local name=$1
	shift
	for _ in $(seq "$REPEATS"); do
		echo "== $name"
		python3 "$BENCH" --name "$name" --clients "$CLIENTS" --duration "$DURATION" "$@" \
			| tee -a "$OUT/results.jsonl"
	done
}

if [ -z "${SKIP_BUILD:-}" ]; then
	build_c
	build_rust port "$PORT_REF"
	build_rust modern "$MODERN_REF"
	build_rust smooth "$SMOOTH_REF"
fi

rm -f "$OUT/results.jsonl"
bench direct
bench c --proxy "$OUT/bin/c"
bench port --proxy "$OUT/bin/port"
bench modern --proxy "$OUT/bin/modern"
bench smooth-off --proxy "$OUT/bin/smooth"
bench smooth-on --proxy "$OUT/bin/smooth" --userinfo '\smooth\1'
# The same after 100 ms of silence, as a client loading a map produces.
bench smooth-on-pause --proxy "$OUT/bin/smooth" --userinfo '\smooth\1' --pause-ms 100
# A slot-limited uplink (20 ms slots, as cl_delay_packet_upstream_rate 50
# simulates), with and without smoothing.
bench direct-clumped --clump-ms 20
bench smooth-off-clumped --proxy "$OUT/bin/smooth" --clump-ms 20
bench smooth-on-clumped --proxy "$OUT/bin/smooth" --clump-ms 20 --userinfo '\smooth\1'

{
	echo "# Results"
	echo
	echo "Built from: C $(cat "$OUT/src/c.rev"), port $(cat "$OUT/src/port.rev"), modern $(cat "$OUT/src/modern.rev"), smooth $(cat "$OUT/src/smooth.rev"). $CLIENTS clients at 77 packets/s for $DURATION s, median of $REPEATS runs. $(date -u +%Y-%m-%d)."
	echo
	python3 "$BENCH" --report "$OUT/results.jsonl"
} > "$OUT/results.md"
echo "== wrote $OUT/results.md"
