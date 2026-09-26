#!/usr/bin/env python3
"""Load generator and resource sampler for qwfwd builds.

Runs a fake QuakeWorld server that echoes game packets, connects a number of
simulated clients through a proxy (or straight to the server, as the
baseline), has each client send game packets at the QuakeWorld rate for a
while, and reports round-trip latency, loss, the regularity of arrivals at
the server, and the proxy's CPU time, context switches and peak memory read
from /proc. One run prints one JSON object; `--report` turns a file of them
into a Markdown table. See README.md in this directory.

Only the standard library is used, so the harness runs anywhere Python 3.9+
and Linux's /proc are available.
"""

import argparse
import heapq
import json
import math
import os
import platform
import select
import shutil
import socket
import statistics
import struct
import subprocess
import sys
import tempfile
import threading
import time

OOB = b"\xff\xff\xff\xff"
NETCHAN_HEADER = 10
# Payload of a game packet: marker, client id, sequence, send time (ns).
# Warm-up probes carry their own marker so their echoes are never measured.
PAYLOAD = struct.Struct("<cHIQ")
MARKER = b"#"
PROBE = b"~"


class EchoServer(threading.Thread):
    """A QuakeWorld server that accepts every connection and echoes game
    packets back untouched, recording when each client's packets arrive."""

    def __init__(self):
        super().__init__(daemon=True)
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.sock.bind(("127.0.0.1", 0))
        self.sock.settimeout(0.1)
        self.port = self.sock.getsockname()[1]
        self.stop = threading.Event()
        self.lock = threading.Lock()
        self.last_arrival = {}
        self.gaps = {}  # client id -> [count, sum, sumsq] of arrival gaps in ms
        self.recording = False

    def run(self):
        while not self.stop.is_set():
            try:
                data, addr = self.sock.recvfrom(65535)
            except socket.timeout:
                continue
            if data.startswith(OOB):
                body = data[4:]
                if body.startswith(b"getchallenge"):
                    self.sock.sendto(OOB + b"c777\0", addr)
                elif body.startswith(b"connect "):
                    self.sock.sendto(OOB + b"j", addr)
                continue
            if len(data) < NETCHAN_HEADER + PAYLOAD.size:
                continue  # keepalive or other header-only traffic
            marker, client, _seq, _sent = PAYLOAD.unpack_from(data, NETCHAN_HEADER)
            if marker not in (MARKER, PROBE):
                continue
            now = time.monotonic_ns()
            if self.recording and marker == MARKER:
                with self.lock:
                    prev = self.last_arrival.get(client)
                    if prev is not None:
                        gap = (now - prev) / 1e6
                        acc = self.gaps.setdefault(client, [0, 0.0, 0.0])
                        acc[0] += 1
                        acc[1] += gap
                        acc[2] += gap * gap
                    self.last_arrival[client] = now
            self.sock.sendto(data, addr)

    def arrival_gap_sd(self):
        """Mean over clients of the standard deviation of inter-arrival gaps, ms."""
        with self.lock:
            sds = []
            for count, total, sumsq in self.gaps.values():
                if count < 2:
                    continue
                mean = total / count
                var = max(sumsq / count - mean * mean, 0.0)
                sds.append(math.sqrt(var))
        return statistics.mean(sds) if sds else float("nan")


class Client:
    def __init__(self, index, target):
        self.index = index
        self.qport = index + 1
        self.target = target
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.sock.bind(("127.0.0.1", 0))
        self.sock.setblocking(False)
        self.seq = 0
        self.sent = 0
        self.received = 0
        self.rtts = []
        self.timeline = None  # (receive time, rtt) when a timeline is wanted

    def ask(self, packet, expect, timeout=1.0):
        """Sends an out-of-band packet and waits for a reply with the given prefix."""
        deadline = time.monotonic() + timeout
        self.sock.sendto(packet, self.target)
        while time.monotonic() < deadline:
            ready, _, _ = select.select([self.sock], [], [], deadline - time.monotonic())
            if not ready:
                break
            data, _ = self.sock.recvfrom(65535)
            if data.startswith(expect):
                return data
        return None

    def handshake(self, server_port, userinfo_extra):
        for _attempt in range(5):
            reply = self.ask(OOB + b"getchallenge\n", OOB + b"c")
            if reply is None:
                continue
            challenge = reply[5:].split(b"\0", 1)[0]
            userinfo = b"\\name\\bench%d\\prx\\127.0.0.1:%d%s" % (
                self.index,
                server_port,
                userinfo_extra.encode(),
            )
            connect = b'%sconnect 28 %d %s "%s"\n' % (OOB, self.qport, challenge, userinfo)
            if self.ask(connect, OOB + b"j") is not None:
                return True
        return False

    def game_packet(self, now_ns, marker=MARKER):
        self.seq += 1
        header = struct.pack("<IIH", self.seq, 0, self.qport)
        return header + PAYLOAD.pack(marker, self.index, self.seq, now_ns)

    def drain(self):
        """Reads everything waiting on the socket, records the round trip of
        each echoed packet, and returns whether a probe echo was among them."""
        probed = False
        while True:
            try:
                data, _ = self.sock.recvfrom(65535)
            except BlockingIOError:
                return probed
            if len(data) < NETCHAN_HEADER + PAYLOAD.size:
                continue
            marker, client, _seq, sent = PAYLOAD.unpack_from(data, NETCHAN_HEADER)
            if client != self.index:
                continue
            if marker == PROBE:
                probed = True
            elif marker == MARKER:
                self.received += 1
                now = time.monotonic_ns()
                self.rtts.append((now - sent) / 1e6)
                if self.timeline is not None:
                    self.timeline.append((now, (now - sent) / 1e6))


class ProcSampler:
    """CPU time, context switches and memory of a process from /proc."""

    def __init__(self, pid):
        self.pid = pid

    def cpu_seconds(self):
        """Time on CPU over all threads, from the scheduler's nanosecond
        counters rather than the 10 ms clock ticks in /proc/pid/stat."""
        total_ns = 0
        for task in os.listdir(f"/proc/{self.pid}/task"):
            try:
                with open(f"/proc/{self.pid}/task/{task}/schedstat") as f:
                    total_ns += int(f.read().split()[0])
            except OSError:
                pass  # a thread that exited between listing and reading
        return total_ns / 1e9

    def status(self):
        out = {}
        with open(f"/proc/{self.pid}/status") as f:
            for line in f:
                key, _, value = line.partition(":")
                out[key] = value.strip()
        return out

    def snapshot(self):
        status = self.status()
        return {
            "cpu": self.cpu_seconds(),
            "ctxt": int(status["voluntary_ctxt_switches"])
            + int(status["nonvoluntary_ctxt_switches"]),
            "rss_kb": int(status["VmRSS"].split()[0]),
            "hwm_kb": int(status["VmHWM"].split()[0]),
            "threads": int(status["Threads"]),
        }


def start_proxy(binary, port, workdir, extra_config):
    cfg_dir = os.path.join(workdir, "qwfwd")
    os.makedirs(cfg_dir, exist_ok=True)
    with open(os.path.join(cfg_dir, "qwfwd.cfg"), "w") as f:
        f.write(
            'set hostname "bench"\n'
            'set masters ""\n'
            "set masters_query 0\n"
            "set masters_heartbeat 0\n"
            "set maxclients 256\n"
            + extra_config
        )
    log = open(os.path.join(workdir, "proxy.log"), "wb")
    # stdin is a pipe that never delivers anything: the C original polls a
    # non-terminal stdin and must not see EOF, the Rust port ignores it.
    proc = subprocess.Popen(
        [os.path.abspath(binary), str(port), "127.0.0.1"],
        cwd=workdir,
        stdin=subprocess.PIPE,
        stdout=log,
        stderr=subprocess.STDOUT,
    )
    return proc


def run(args):
    server = EchoServer()
    server.start()
    workdir = tempfile.mkdtemp(prefix="qwfwd-bench-")
    proxy = None
    if args.target:
        host, _, port = args.target.rpartition(":")
        target = (host or "127.0.0.1", int(port))
    elif args.proxy:
        port = free_port()
        proxy = start_proxy(args.proxy, port, workdir, args.config.replace("\\n", "\n"))
        target = ("127.0.0.1", port)
        time.sleep(0.5)
        if proxy.poll() is not None:
            sys.exit(f"proxy exited with {proxy.returncode}, see {workdir}/proxy.log")
    else:
        target = ("127.0.0.1", server.port)

    clients = [Client(i, target) for i in range(args.clients)]
    if args.timeline:
        for client in clients:
            client.timeline = []
    for client in clients:
        if not client.handshake(server.port, args.userinfo):
            sys.exit(f"client {client.index} could not connect, see {workdir}/proxy.log")
    interval_ns = int(1e9 / args.rate)
    clump_ns = int(args.clump_ms * 1e6)
    pause_ns = int(args.pause_ms * 1e6)
    sampler = ProcSampler(proxy.pid) if proxy else None
    before = None

    # Each client generates packets at the QuakeWorld rate from its own
    # phase, staggered so the clients do not all send at once. They are
    # warm-up probes until every client has had an echo back, which means
    # the proxy's own connections to the server are up; the measured traffic
    # then continues on the same cadence, so a proxy measuring a client's
    # rate sees no gap at the hand-over (unless a pause is asked for). With
    # clumping, a packet leaves at the next multiple of the clump interval
    # instead, the way a slot-limited uplink behaves.
    t0 = time.monotonic_ns()
    generate = [(t0 + (i * interval_ns) // len(clients), i) for i in range(len(clients))]
    heapq.heapify(generate)
    transmit = []
    socks = [c.sock for c in clients]
    by_sock = {c.sock: c for c in clients}
    warm = set()
    start = end = None
    warm_deadline = t0 + 5_000_000_000
    while True:
        now = time.monotonic_ns()
        if start is None:
            if len(warm) == len(clients):
                start = now + pause_ns
                end = start + int(args.duration * 1e9)
                before = sampler.snapshot() if sampler else None
                server.recording = True
            elif now > warm_deadline:
                cold = min(set(range(len(clients))) - warm)
                sys.exit(f"client {cold} got no echo, see {workdir}/proxy.log")
        while generate and generate[0][0] <= now:
            t_gen, i = heapq.heappop(generate)
            measured = start is not None and t_gen >= start
            if start is None or measured:  # silent during a pause
                t_tx = t_gen if not clump_ns else -(-t_gen // clump_ns) * clump_ns
                heapq.heappush(transmit, (t_tx, i, measured))
            if end is None or t_gen + interval_ns < end:
                heapq.heappush(generate, (t_gen + interval_ns, i))
        while transmit and transmit[0][0] <= now:
            _, i, measured = heapq.heappop(transmit)
            client = clients[i]
            marker = MARKER if measured else PROBE
            client.sock.sendto(client.game_packet(time.monotonic_ns(), marker), target)
            if measured:
                client.sent += 1
        if not generate and not transmit:
            break
        horizon = now + 1_000_000_000
        next_event = min(
            generate[0][0] if generate else horizon,
            transmit[0][0] if transmit else horizon,
        )
        timeout = max(next_event - time.monotonic_ns(), 0) / 1e9
        ready, _, _ = select.select(socks, [], [], min(timeout, 0.005))
        for sock in ready:
            client = by_sock[sock]
            if client.drain():
                warm.add(client.index)
    # Let the last echoes come back.
    settle = time.monotonic() + 0.5
    while time.monotonic() < settle:
        ready, _, _ = select.select(socks, [], [], 0.05)
        for sock in ready:
            by_sock[sock].drain()
    wall = (time.monotonic_ns() - start) / 1e9
    server.recording = False
    after = sampler.snapshot() if sampler else None

    if proxy:
        proxy.terminate()
        proxy.wait(timeout=5)
    server.stop.set()
    if not args.keep:
        shutil.rmtree(workdir, ignore_errors=True)

    if args.timeline:
        print_timeline(clients, start)
    rtts = sorted(r for c in clients for r in c.rtts)
    sent = sum(c.sent for c in clients)
    received = sum(c.received for c in clients)
    result = {
        "name": args.name,
        "proxy": args.target or (os.path.basename(args.proxy) if args.proxy else "none"),
        "clients": args.clients,
        "rate": args.rate,
        "clump_ms": args.clump_ms,
        "pause_ms": args.pause_ms,
        "userinfo": args.userinfo,
        "duration_s": round(wall, 2),
        "sent": sent,
        "received": received,
        "loss_pct": round(100.0 * (sent - received) / sent, 3) if sent else None,
        "rtt_mean_ms": round(statistics.mean(rtts), 3) if rtts else None,
        "rtt_p50_ms": round(percentile(rtts, 50), 3) if rtts else None,
        "rtt_p99_ms": round(percentile(rtts, 99), 3) if rtts else None,
        "rtt_max_ms": round(rtts[-1], 3) if rtts else None,
        "server_gap_sd_ms": round(server.arrival_gap_sd(), 3),
    }
    if sampler:
        cpu = after["cpu"] - before["cpu"]
        result.update(
            {
                "cpu_pct": round(100.0 * cpu / wall, 2),
                "cpu_us_per_packet": round(1e6 * cpu / max(sent + received, 1), 2),
                "ctxt_per_s": round((after["ctxt"] - before["ctxt"]) / wall, 1),
                "threads": after["threads"],
                "rss_mb": round(after["rss_kb"] / 1024, 2),
                "peak_rss_mb": round(after["hwm_kb"] / 1024, 2),
            }
        )
    print(json.dumps(result))


def print_timeline(clients, start_ns):
    """Per-second round-trip average and maximum over all clients, to stderr."""
    buckets = {}
    for client in clients:
        for at, rtt in client.timeline:
            second = (at - start_ns) // 1_000_000_000
            buckets.setdefault(second, []).append(rtt)
    print("second  rtt avg  rtt max  packets", file=sys.stderr)
    for second in sorted(buckets):
        rtts = buckets[second]
        print(f"{second:6d} {statistics.mean(rtts):8.2f} {max(rtts):8.2f} {len(rtts):8d}", file=sys.stderr)


def percentile(sorted_values, pct):
    if not sorted_values:
        return float("nan")
    k = (len(sorted_values) - 1) * pct / 100.0
    lo, hi = math.floor(k), math.ceil(k)
    if lo == hi:
        return sorted_values[lo]
    return sorted_values[lo] + (sorted_values[hi] - sorted_values[lo]) * (k - lo)


def free_port():
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


COLUMNS = [
    ("name", "run", "{}"),
    ("runs", "runs", "{}"),
    ("clients", "clients", "{:.0f}"),
    ("clump_ms", "clump ms", "{:.0f}"),
    ("pause_ms", "pause ms", "{:.0f}"),
    ("sent", "sent", "{:.0f}"),
    ("loss_pct", "loss %", "{:.2f}"),
    ("rtt_mean_ms", "rtt avg", "{:.3f}"),
    ("rtt_p50_ms", "p50", "{:.3f}"),
    ("rtt_p99_ms", "p99", "{:.3f}"),
    ("rtt_max_ms", "max", "{:.2f}"),
    ("server_gap_sd_ms", "arrival sd", "{:.2f}"),
    ("cpu_pct", "cpu %", "{:.1f}"),
    ("cpu_us_per_packet", "µs/pkt", "{:.2f}"),
    ("ctxt_per_s", "ctxt/s", "{:.0f}"),
    ("threads", "thr", "{:.0f}"),
    ("peak_rss_mb", "peak rss MB", "{:.1f}"),
]


def report(path):
    """Rows with the same name are repeats: each numeric column is their median."""
    by_name = {}
    for line in open(path):
        if line.strip():
            row = json.loads(line)
            by_name.setdefault(row["name"], []).append(row)
    rows = []
    for name, group in by_name.items():
        merged = {"name": name, "runs": len(group)}
        for key in group[0]:
            values = [r[key] for r in group if isinstance(r.get(key), (int, float))]
            if values:
                merged[key] = statistics.median(values)
            elif key not in merged:
                merged[key] = group[0][key]
        rows.append(merged)
    print("| " + " | ".join(title for _, title, _ in COLUMNS) + " |")
    print("|" + "|".join("---" if key == "name" else "---:" for key, _, _ in COLUMNS) + "|")
    for row in rows:
        cells = []
        for key, _, fmt in COLUMNS:
            value = row.get(key)
            cells.append("" if value is None else fmt.format(value))
        print("| " + " | ".join(cells) + " |")
    print()
    print(
        f"Latency in ms, round trip client to server and back. "
        f"{platform.node()}: {cpu_model()}, {platform.system()} {platform.release()}, "
        f"Python {platform.python_version()}."
    )


def cpu_model():
    try:
        with open("/proc/cpuinfo") as f:
            for line in f:
                if line.startswith("model name"):
                    return line.split(":", 1)[1].strip()
    except OSError:
        pass
    return platform.processor()


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--name", default="run", help="label for the result row")
    parser.add_argument("--proxy", default="", help="proxy binary; empty for the direct baseline")
    parser.add_argument("--target", default="", help="host:port of an already running proxy (no resource sampling)")
    parser.add_argument("--clients", type=int, default=16)
    parser.add_argument("--rate", type=float, default=77.0, help="packets per second per client")
    parser.add_argument("--duration", type=float, default=20.0, help="seconds of traffic")
    parser.add_argument("--clump-ms", type=float, default=0.0, help="uplink slot length; 0 sends on time")
    parser.add_argument("--pause-ms", type=float, default=0.0, help="silence between warm-up and traffic")
    parser.add_argument("--userinfo", default="", help="extra userinfo, e.g. '\\smooth\\1'")
    parser.add_argument("--config", default="", help="extra lines for qwfwd.cfg (\\n separated)")
    parser.add_argument("--keep", action="store_true", help="keep the work directory with the proxy log")
    parser.add_argument("--timeline", action="store_true", help="print per-second latency to stderr")
    parser.add_argument("--report", metavar="RESULTS.jsonl", help="print a Markdown table instead")
    args = parser.parse_args()
    if args.report:
        report(args.report)
    else:
        run(args)


if __name__ == "__main__":
    main()
