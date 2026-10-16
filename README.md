# treason

A MIDI sequencer driven by an AI agent. You describe what you want (a drum
pattern, a bassline, a change of feel) and the agent builds it with the
sequencer's tools. treason plays it as MIDI to external gear: drum machines,
synths and other sequencers. It makes no sound itself.

Status: early. `treason serve` plays what an agent writes with step
patterns, such as drum parts, on the MIDI ports in `treason.toml`. Ports
with `clock = true` get MIDI clock and Start, Stop and Continue, so their
gear plays along. Still to come: per-port latency and melodic parts written
with pitch names. Until then, the `latency_ms` setting has no effect. See
[ARCHITECTURE.md](ARCHITECTURE.md) for the design and for what's still a
stub.

## Requirements

- Rust 1.95, pinned in `rust-toolchain.toml`. With
  [rustup](https://rustup.rs) installed and its `~/.cargo/bin` first on
  your `PATH`, `cargo` picks it up automatically.
- Linux for now, with the ALSA development package for MIDI output:
  `sudo apt install libasound2-dev`.

## Usage

```sh
cargo run -- devices        # the device profiles in ./devices
cargo run -- ports          # the MIDI output ports on this machine
cargo run -- serve          # MCP over HTTP at http://127.0.0.1:6434/mcp
cargo run -- serve --stdio  # MCP over stdin and stdout, for one agent
```

Settings live in `treason.toml`; the example in this directory explains
each one. To connect Claude Code to a running `treason serve`:

```sh
claude mcp add --transport http treason http://127.0.0.1:6434/mcp
```

Each `[[ports]]` entry opens an output port. With `connect`, treason
connects it to the first port whose name contains that text. Without it,
the port is virtual and you connect it to your gear, with `aconnect` or a
patchbay on Linux. To see what treason sends on its first port:

```sh
aseqdump -p treason:0
```

Ctrl-C or SIGTERM stops `treason serve`, and every sounding note ends first.

## Development

```sh
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --check
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
```

CI runs the same checks on every pull request.
[AGENTS.md](AGENTS.md) has the rules for contributing, for people and
agents alike.
