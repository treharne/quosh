# Mosh prediction oracle

`crates/quosh-predict` is a clean-room port of Mosh's `PredictionEngine`. This
directory builds Mosh's *unmodified* engine into a small driver so the port can
be compared against the original on identical input.

It is a local development tool. The Mosh checkout (`reference/mosh`, see
`docs/07-mosh-reference.md`) is not vendored, so neither this tool nor its
outputs run in CI — the generated golden traces under
`crates/quosh-predict/tests/fixtures/` are committed instead.

## Build

Needs the Mosh source and `g++`:

```sh
MOSH_REF=/path/to/mosh tools/mosh-oracle/build.sh
# default: reference/mosh, output /tmp/quosh-mosh-oracle/oracle
```

`build.sh` copies `terminaloverlay.{h,cc}` into a scratch tree and removes the
two `src/network/*` includes the prediction engine does not use (they pull in
protobuf). It supplies `timestamp()` and `Network::ACK_INTERVAL` itself.

## Run

The oracle reads one command per line on stdin and dumps a grid after every
command that changes or renders state:

| Command | Meaning |
|---|---|
| `FEED <hex>` | server output, fed to the authoritative emulator |
| `KEY <hex>` | user keystrokes, one `new_user_byte` per byte |
| `SENT <n>` | set `local_frame_sent` |
| `EARLY <n>` | set the transport ack |
| `LATE <n>` | set the echo ack |
| `RTT <n>` | set `send_interval` (ms) |
| `TICK <n>` | advance the fake clock |
| `RESET` / `PRED adaptive\|always\|never` | control the engine |

Output per render is `CUR r c`, `ROW r <text>`, `UL r <bitmap>`, `END`.

## Regenerate the Rust fixtures

```sh
tools/mosh-oracle/build.sh
tools/mosh-oracle/generate.sh    # writes crates/quosh-predict/tests/fixtures/*.golden
cargo test -p quosh-predict --test oracle
```

Only regenerate when the port intentionally changes behaviour, and say why.
