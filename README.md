# treason

A MIDI sequencer driven by an AI agent. You describe what you want (a drum
pattern, a bassline, a change of feel) and the agent builds it with the
sequencer's tools. treason plays it as MIDI to external gear: drum machines,
synths and other sequencers. It makes no sound itself.

Status: early. `treason serve` plays what an agent writes on the MIDI ports
in `treason.toml`: drum parts as step patterns, melodic parts as notes with
pitch names such as `C4`, and controls such as a synth's mod wheel or
filter cutoff as values that jump or ramp. Ports with `clock = true` get
MIDI clock and Start, Stop and Continue, so their gear plays along. A
port's `latency_ms` makes up for its gear's delay, so everything sounds
together. On Linux, `backend = "alsa-seq"` hands messages to the kernel's
sequencer ahead of time, which keeps the timing within a millisecond even
when the computer is busy. An agent saves the project by name and loads it
back later, with everything it holds, and exports it as a MIDI file that
opens in a DAW. See [ARCHITECTURE.md](ARCHITECTURE.md) for the design and
for what's still a stub.

## Docs

- [docs/guide.md](docs/guide.md): how to drive treason, for agents and
  people alike. It covers projects, tracks and loops, every tool, the
  notations and limits, and connecting gear, with examples of every call.
  Agents read it over MCP with `get_guide`.
- [docs/tools.md](docs/tools.md): every tool's inputs and outputs, made
  from the tools themselves.
- [ARCHITECTURE.md](ARCHITECTURE.md): the design and what comes next.
- [AGENTS.md](AGENTS.md): the rules for contributing.

## Requirements

- Rust 1.95, pinned in `rust-toolchain.toml`. With
  [rustup](https://rustup.rs) installed and its `~/.cargo/bin` first on
  your `PATH`, `cargo` picks it up automatically.
- Linux for now, with the ALSA development package for MIDI output:
  `sudo apt install libasound2-dev`.

## Usage

```sh
cargo run -- devices              # the device profiles in ./devices
cargo run -- ports                # the MIDI output ports on this machine
cargo run -- projects             # the saved projects, newest first
cargo run -- serve                # MCP over HTTP at http://127.0.0.1:6434/mcp
cargo run -- serve --stdio        # MCP over stdin and stdout, for one agent
cargo run -- serve --project jam  # start with the saved project "jam"
cargo run -- export jam           # the saved project "jam" as exports/jam.mid
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
The project lives in memory until an agent saves it, so ask for a save
before you stop: `save_project` writes it to `projects/<name>.toml`, and
`load_project` or `serve --project` brings it back with its tempo, time
signature and tracks, each with its clip, port, channel, device and mute.
`status` says whether there are unsaved changes.

A track can also move its gear's controls (MIDI CC) with `set_controls`,
alongside its notes or on its own, such as modulation for a synth that
plays its own notes. The agent names a control by the device's parameter,
by a standard name such as `modulation`, `volume` or `cutoff`, or by its
number, and gives it points that jump or ramp, smoothly enough for an LFO.
Stop and track cleanup release sustain and sostenuto holds, even after a
note's key has ended. Pedals affect a whole MIDI channel, so tracks sharing
that channel share its pedal release. Panic also sends All Sound Off.

`export_midi` writes the project as a MIDI file, `exports/<name>.mid`, with
the tempo, the time signature and a track for each unmuted track, and
`treason export` does the same for a saved project. By default the file runs
until every loop ends on the same bar line, so each pattern is complete;
`bars` (or `--bars`) sets the length, and `--output` writes elsewhere.

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
