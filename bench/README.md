# Performance benchmark

A reproducible comparison of the proxy builds: the C original, the plain
Rust port, the modernized Rust port, and the smoothing build with smoothing
off and on. It measures what a server operator would ask about a proxy:
how much CPU and memory it needs for a given number of players, and how
much latency it adds to each packet.

It is not an end-to-end test with real game clients. Everything runs on one
machine over loopback, so the absolute latencies are far below anything
seen on a real link; what the numbers show is the difference between builds
under identical load.

## How it works

`bench.py` starts a fake QuakeWorld server that accepts every connection
and echoes game packets back untouched, then connects a number of
simulated clients through the proxy under test (or straight to the server,
for the baseline). Each client does the real handshake, `getchallenge` and
`connect` with the `prx` userinfo key, and then sends game packets at
QuakeWorld's 77 packets/s, each from its own phase so the clients do not
all send at once. The first packets are warm-up probes; once every client
has had an echo back, which means the proxy's own connections to the server
are up, the measured traffic continues on the same cadence, so a proxy that
measures a client's rate sees no gap at the hand-over. Every packet carries
its send time, so when its echo comes back the client measures the round
trip through the proxy and back, twice through the forwarding path. The
server records the gaps between successive arrivals from each client, whose
standard deviation shows how evenly packets reach it.

With `--clump-ms N` the clients behave like a slot-limited uplink: packets
are still generated at 77 packets/s but only leave at the next multiple of
`N` ms, so they reach the proxy in clumps. This is the case connection
smoothing exists for; ezQuake's `cl_delay_packet_upstream_rate` simulates
the same thing on a real client. With `--pause-ms N` the clients fall
silent for `N` ms between warm-up and traffic, as a client loading a map
does.

While the traffic runs, the harness reads the proxy's CPU time (from the
scheduler's nanosecond counters, summed over threads), context switches and
peak resident set from `/proc`. Each run prints one JSON object; `--report`
turns a file of them into a Markdown table, taking the median over runs
with the same name. `--timeline` prints per-second latency to stderr, which
is how to see a transient; `--target host:port` drives a proxy started by
hand, for instance on a terminal where `clstats` can be watched.

`run.sh` builds every variant from its git ref into a scratch directory
(the C original with CMake, the Rust builds in release mode, each in its
own cargo target directory) and runs the matrix:

| run | what |
|---|---|
| `direct` | clients talk to the server directly: the floor for latency on this machine |
| `c` | the C original |
| `port` | the plain Rust port |
| `modern` | the modernized Rust port |
| `smooth-off` | the smoothing build, clients not opted in |
| `smooth-on` | the smoothing build, clients opted in with `setinfo smooth 1` |
| `smooth-on-pause` | the same after 100 ms of silence, as a map load produces |
| `*-clumped` | the same through a 20 ms uplink slot, with and without smoothing |

Every run is repeated (three times by default) and the table shows the
median of each column.

## Running it

```bash
bench/run.sh                       # 16 clients, 20 s per run, 3 repeats, results in target/bench
CLIENTS=64 DURATION=30 REPEATS=5 bench/run.sh
SKIP_BUILD=1 bench/run.sh          # reuse the binaries from the last build
python3 bench/bench.py --proxy target/release/qwfwd --clients 16 --duration 10
python3 bench/bench.py --proxy target/release/qwfwd --userinfo '\smooth\1' --timeline
```

Needs Python 3.9+, Linux (for `/proc`), CMake and a C compiler for the
original, and a Rust toolchain. Refs default to the last commit of the C
original, the port and the modernization as merged, and `master` for the
smoothing build, and can be overridden with `C_REF`, `PORT_REF`,
`MODERN_REF` and `SMOOTH_REF`.

## Reading the numbers

* **rtt** is the round trip in ms as seen by a client: client to proxy to
  server, echoed, server to proxy to client. Subtract the `direct` row to
  get what the proxy adds, for two traversals.
* **arrival sd** is the standard deviation of the gaps between a client's
  packets as they reach the server, averaged over clients, in ms. Steady
  77 packets/s gives a value near zero; a 20 ms uplink slot gives around
  9 ms, and smoothing should bring that back down.
* **cpu %** is the proxy's CPU time divided by wall time, so 100 is one
  core. **µs/pkt** is the same time divided by the packets the proxy
  handled in both directions, which scales better across client counts.
* **ctxt/s** counts the proxy's context switches per second, a proxy for
  how often it wakes up.
* **peak rss** is the proxy's high-water resident set in MB.

## Results

Built from the C original at 576214f, the port as merged (bcff74e), the
modernization as merged (53af5c5) and the smoothing build with the
high-resolution timer and rate snap (c8d4c28, branch `hires-timer`) with
`bench/run.sh` defaults: 16 clients at 77 packets/s for 20 s, median of
3 runs, on 2026-09-28. The `master-*` rows are the smoothing build as
merged on master (f15f4de), before the timer and snap, run the same way
for comparison, and the `*-64-*` rows repeat the smoothing rows with 64
clients, median of 2 runs.

| run | runs | clients | clump ms | pause ms | sent | loss % | rtt avg | p50 | p99 | max | arrival sd | cpu % | µs/pkt | ctxt/s | thr | peak rss MB |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| direct | 3 | 16 | 0 | 0 | 24640 | 0.00 | 0.092 | 0.066 | 0.312 | 1.01 | 0.07 |  |  |  |  |  |
| c | 3 | 16 | 0 | 0 | 24640 | 0.00 | 0.151 | 0.130 | 0.413 | 1.51 | 0.07 | 6.8 | 28.07 | 2425 | 1 | 2.4 |
| port | 3 | 16 | 0 | 0 | 24640 | 0.00 | 0.141 | 0.130 | 0.409 | 1.35 | 0.07 | 6.0 | 24.83 | 2414 | 1 | 3.0 |
| modern | 3 | 16 | 0 | 0 | 24640 | 0.00 | 0.151 | 0.125 | 0.418 | 0.82 | 0.07 | 6.0 | 24.83 | 2417 | 1 | 3.1 |
| smooth-off | 3 | 16 | 0 | 0 | 24640 | 0.00 | 0.151 | 0.124 | 0.419 | 1.57 | 0.07 | 6.2 | 25.90 | 2422 | 1 | 3.9 |
| smooth-on | 3 | 16 | 0 | 0 | 24640 | 0.00 | 0.285 | 0.272 | 0.580 | 0.87 | 0.05 | 8.1 | 33.83 | 3576 | 1 | 4.3 |
| smooth-on-pause | 3 | 16 | 0 | 100 | 24640 | 0.00 | 3.239 | 0.320 | 35.662 | 42.46 | 0.28 | 8.2 | 34.28 | 3521 | 1 | 4.3 |
| direct-clumped | 3 | 16 | 20 | 0 | 24640 | 0.00 | 0.742 | 0.616 | 2.067 | 3.40 | 9.51 |  |  |  |  |  |
| smooth-off-clumped | 3 | 16 | 20 | 0 | 24640 | 0.00 | 0.840 | 0.781 | 2.147 | 3.26 | 9.51 | 3.6 | 14.93 | 652 | 1 | 4.0 |
| smooth-on-clumped | 3 | 16 | 20 | 0 | 24640 | 0.00 | 9.710 | 9.825 | 19.754 | 21.12 | 0.11 | 7.7 | 31.93 | 2674 | 1 | 4.3 |
| master-smooth-off | 3 | 16 | 0 | 0 | 24640 | 0.00 | 0.154 | 0.129 | 0.425 | 0.77 | 0.08 | 6.3 | 26.23 | 2416 | 1 | 3.9 |
| master-smooth-on | 3 | 16 | 0 | 0 | 24640 | 0.00 | 1.961 | 1.825 | 3.433 | 3.90 | 0.36 | 6.8 | 28.07 | 1974 | 1 | 4.2 |
| master-64-smooth-off | 2 | 64 | 0 | 0 | 98560 | 0.00 | 0.147 | 0.127 | 0.504 | 2.84 | 0.10 | 21.4 | 22.28 | 9323 | 1 | 6.6 |
| master-64-smooth-on | 2 | 64 | 0 | 0 | 98560 | 0.00 | 1.898 | 1.901 | 3.094 | 7.69 | 0.27 | 21.7 | 22.59 | 5661 | 1 | 7.7 |
| smooth-64-smooth-off | 2 | 64 | 0 | 0 | 98560 | 0.00 | 0.147 | 0.129 | 0.478 | 2.15 | 0.09 | 22.2 | 23.11 | 9327 | 1 | 6.6 |
| smooth-64-smooth-on | 2 | 64 | 0 | 0 | 98560 | 0.00 | 0.337 | 0.315 | 0.831 | 3.77 | 0.07 | 28.6 | 29.78 | 11010 | 1 | 7.6 |

Latency in ms, round trip client to server and back. matt-desktop:
Intel Core i7-14700F, Linux 7.2.3, Python 3.14.

### What the numbers say

**Latency.** Every proxy adds about 55 µs to the round trip, so roughly
27 µs per traversal, and the C original and the three Rust builds are
within a few microseconds of each other: the forwarding path is not where
time goes. Against a real link's tens of milliseconds this is nothing.

**CPU.** Per packet handled, the plain port and the modernized port cost
about 12 % less than the C original (25 versus 28 µs), and the smoothing
build with no client opted in the same as the port. At 16 players each
build needs 6 to 7 % of one core, and the cost is almost entirely the
wake-up per packet (the context switch column matches the packet rate),
so it scales with packets rather than with what is done to them. The
clumped rows show this from the other side: when two packets arrive per
wake-up, the cost per packet halves.

**Memory.** The C original holds 2.4 MB resident, the Rust builds 3 to
3.1 MB, and the smoothing build about 4 MB because it keeps five seconds of
timing samples per client for `clstats` and the rate estimate; 6.6 to
7.7 MB at 64 clients.

**Smoothing on a clumped link.** With packets leaving in 20 ms slots the
server sees arrival gaps with a 9.5 ms standard deviation; smoothing brings
that to 0.1 ms, at the cost of holding packets for about half a slot, 10 ms
on average and 20 ms at the 99th percentile. That is the intended trade.

**Smoothing on a clean link, and the high-resolution timer.** The
smoothing build as first merged waited for release slots with tokio's
timer, whose 1 ms resolution rounds every deadline up; an opted-in client
on a clean link paid about 1.9 ms and its arrivals at the server were
less regular than without a proxy (`master-smooth-on`). With the kernel's
high-resolution timer and the rate snap (`smooth-on`) that client pays
0.13 ms over an unsmoothed one, its 99th percentile is 0.58 ms, and its
arrivals are as regular as on the direct link. The price is that every
release is its own wake-up where the millisecond rounding used to batch
a few: about 30 % more CPU per packet for smoothed clients, 1.3 points of
a core at 16 smoothed clients and 7 points at 64. Unsmoothed clients cost
exactly what they did (`smooth-off` against `master-smooth-off`, at both
16 and 64 clients).

**A silence before traffic is still costly.** `smooth-on-pause` sends the
same steady traffic after 100 ms of silence, which is what a client
loading a map produces. That single gap sits in the five second rate
window, raises the mean arrival gap and so the release interval by
several percent, and the queue grows into catch-up: waits reach 42 ms and
the 99th percentile is 36 ms until the gap ages out of the window about
five seconds later. The rate snap softens this (it was 64 and 59 ms
before) but does not remove it. `--timeline` shows the shape: fine in the
first second on the configured interval, a spike as the measured interval
takes over, then settling. Two changes would fix it and are worth doing
before smoothing is relied on right after a map change: leave gaps well
above the client's interval out of the rate estimate, since they are
stalls rather than rate, and measure the rate over a longer window than
the statistics, since a client's rate is constant.

## Caveats

* The load generator is a single Python thread. It keeps up comfortably at
  the default 16 clients (about 1,200 packets/s each way) and at 64, but it
  adds its own scheduling jitter to every measurement. The `direct` row
  carries the same jitter, so differences between rows are meaningful even
  where the absolute values are not.
* CPU figures for a process that spends most of its time asleep are
  dominated by wake-up cost, and on loopback with no real network latency
  every packet is a wake-up. Real deployments spend far less per packet
  than these figures suggest; the ratios between builds still hold.
* Smoothing under clumped input is expected to raise round-trip latency:
  holding early packets until their slot is the whole point. The gain shows
  in the arrival sd column, not in rtt.
* Results depend on the machine and on whatever else it is doing. The
  results below name the hardware; rerun `run.sh` to get figures for yours.
