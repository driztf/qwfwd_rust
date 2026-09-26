# QWFWD: a QuakeWorld server proxy


## Supported architectures

The following architectures are fully supported by **[QTV][qtv]** and are available as prebuilt binaries:
* Linux amd64 (Intel and AMD 64-bits processors)
* Linux i686 (Intel and AMD 32-bit processors)
* Linux aarch (ARM 64-bit processors)
* Linux armhf (ARM 32-bit processors)
* Windows x64 (Intel and AMD 64-bits processors)
* Windows x86 (Intel and AMD 32-bit processors)

## Prebuilt binaries
You can find the prebuilt binaries on [this download page][qwfwd-builds].

## Prerequisites

A stable [Rust toolchain](https://rustup.rs/) (edition 2024, Rust 1.85 or newer).

## Building binaries

```bash
cargo build --release
```

The binary is written to ``target/release/qwfwd`` (``qwfwd.exe`` on Windows).

To cross compile, install [cross](https://github.com/cross-rs/cross) and pick a target triple, for example:
```bash
cross build --release --target aarch64-unknown-linux-gnu
```

## Running

```bash
qwfwd [port [ip]] [+command ...]
```

The proxy reads ``qwfwd/qwfwd.cfg`` (see [the example config](resources/example-configs/qwfwd.cfg))
and ``qwfwd_listip.cfg`` on startup; ``+set hostname "my proxy"`` style arguments run after the
config. When attached to a terminal the console accepts commands on stdin with line editing
(arrow keys move through history and along the line, Home/End, Ctrl-A/E and the usual
readline bindings; Ctrl-C quits), and ``SIGHUP`` reloads ``qwfwd.cfg``. Run ``cmdlist`` and
``cvarlist`` in the console for the full list.

## Connection smoothing

Links such as cellular uplinks deliver QuakeWorld's steady 77 packets/s (one
every 13 ms) in clumps. The proxy can re-space a client's game packets before
forwarding them to the server: packets arriving on time pass straight through
and packets arriving early wait for their slot. Slots run at the client's own
send rate, measured from its arrivals over the last few seconds;
``smooth_interval`` ms (default 1000/77) is used until enough have been seen. Any
slack that builds up in the queue beyond what the clumping needs is drained by
shortening slots by at most ``smooth_drain`` percent (10), a backlog older than
``smooth_catchup`` ms drains at double rate, and packets queued longer than ``smooth_maxdelay`` ms are dropped oldest
first. Clients that send every packet twice for loss protection
(``cl_c2sdupe``) are fine: a copy rides with its original and takes no slot
of its own.

The ``smooth`` cvar sets who gets smoothed: ``0`` nobody, ``1`` (default)
clients that opt in with ``setinfo smooth 1``, ``2`` everyone except clients
that set ``setinfo smooth 0``. Clients can change the setting at any time,
also while connected; it takes effect on the next packet.

``clstats`` shows, per client over the last 5 seconds, the average, standard
deviation and maximum gap between packets as they arrive from the client and
as they leave for the server, the time packets spent queued, duplicates and
drops — so you can see a client's jitter before deciding to smooth it. ``cllist`` shows
queue depth and total drops.

Run the proxy close to the server (on the good side of the bad link) for this
to help; a proxy on the far side would only see the clumps after the fact.

## Development

```bash
cargo test
cargo clippy --all-targets
```

## Versioning

For the versions available, see the [tags on this repository][qwfwd-tags].

## Authors

  deurk
  qqshka
  VVD

## Code of Conduct

We try to stick to our code of conduct when it comes to interaction around this project. See the [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md) file for details.

## License

This project is licensed under the GPL-2.0 License - see the [LICENSE.md](LICENSE.md) file for details.

## Acknowledgments

* Thanks to the fine folks on [Quakeworld Discord][discord-qw] for their support and ideas.

[qwfwd]: https://github.com/QW-Group/qwfwd
[qwfwd-tags]: https://github.com/QW-Group/qwfwd/tags
[qwfwd-builds]: https://builds.quakeworld.nu/qwfwd
[discord-qw]: http://discord.quake.world/
