# Agent instructions

treason is a MIDI sequencer driven by an AI agent, written in Rust. It
sends MIDI only, no audio, to drum machines, synths and other sequencers.
[README.md](README.md) covers usage. [ARCHITECTURE.md](ARCHITECTURE.md)
covers the design, which modules are still stubs and what comes next;
read it before you change code.

## Reuse first

- Look for an existing type, trait or helper before writing one. Core
  already has the vocabulary: `Ticks`, `Bpm`, `TimeSignature`, `Channel`,
  `U7`, `MidiMessage`, the project model, and the traits between crates,
  `MidiOutput` and `Playback`. The engine has `Timebase` for time and
  `ActiveNotes` for note-offs.
- Extend what's there rather than adding a near copy. If nothing fits, say
  why in the PR.
- Make the smallest change that is correct and complete. Leave code the
  task doesn't need alone, and call out any change in behavior.

## Architecture

- Crates depend on each other only as the diagram in ARCHITECTURE.md
  shows. `treason-devices`, `treason-engine` and `treason-io` depend on
  `treason-core` alone, `treason-control` on core and devices, and
  `treason-mcp` on control, devices and core. Only the `treason` binary
  wires them all together. A new dependency between crates needs a
  reason in the PR.
- The engine never sees a MIDI backend, and neither the control layer
  nor the MCP server sees the engine. They meet through the `MidiOutput`,
  `Playback` and `MidiExport` traits, which is also what lets tests swap
  in fakes. Keep it that way.
- Put code where it belongs:
  - MIDI, musical time and the project model: core
  - names for a device's notes and controls: devices
  - real-time playback: engine
  - MIDI backends and the MIDI file writer: io, behind a Cargo feature
    if they link a system library
  - edits, undo, publishing, saved projects and exported files: control
  - the agent's tools, their inputs and outputs, notations: mcp
  - CLI, configuration and wiring: the binary
- Add a trait for a second implementation or a test fake, not in case one
  is needed later.

## The engine thread

Code that runs on the engine thread:

1. takes no locks and doesn't allocate or free memory once playing.
   Buffers are allocated up front.
2. waits only for the clock and the MIDI output. No file or network I/O,
   and no logging while playing.
3. talks to the rest of the program only through `EngineLink`: the
   `ArcSwap` snapshot, the `rtrb` command queue and atomics.
4. never leaves a note stuck. Every note-on gets its note-off, including
   on stop, panic, clip changes and shutdown.

Positions are whole ticks (960 per quarter note), and times are integer
nanoseconds computed by `Timebase` from an anchor. Never accumulate
floating-point time.

## Types and boundaries

- Use typed structs and enums at every boundary: MCP, configuration,
  device files, project files, MIDI files. No `serde_json::Value`. A map is fine where
  its keys are open-ended names, such as the voices in `set_steps`.
- The agent's inputs and outputs live in `treason-mcp`'s `dto` module and
  convert explicitly to core types. Inputs reject unknown fields with
  `#[serde(deny_unknown_fields)]`.
- Channels are 1–16 wherever people or agents see them and 0–15 inside;
  `Channel` converts. Bars and beats count from 1 in the agent's tools.
- Library crates have their own error enums, made with `thiserror`.
  `anyhow` is for the binary only.

## Agent tools

The MCP tools are how the agent makes music, so design them for the model
rather than around the internals.

- A tool's doc comment and its input's field docs become the description
  and schema the agent reads. Write them for a model that has never seen
  the code, with an example where it helps.
- Prefer names to numbers: voice names, step strings, pitch names, bars
  counted from 1.
- Reject a bad call with a tool error (`Err(String)`) that says how to fix
  it, such as "there is no port `x`; see list_ports". Never ignore an
  agent's mistake silently.
- Annotate tools that change nothing with `read_only_hint = true`.

## Conventions

- Rust 1.95, pinned in `rust-toolchain.toml`, edition 2024. `unsafe_code`
  is denied.
- Unwritten code is a `todo!()` stub with its final signature and docs,
  marked `#[expect(unused_variables, reason = "not implemented yet")]` (or
  `dead_code`), never `allow`. When you write the code, delete the
  attribute (the compiler warns until you do) and update the module's
  status in ARCHITECTURE.md.
- Log with `tracing`, which writes to stderr. Never print to stdout from
  the server: `treason serve --stdio` speaks MCP over stdout.
- Over HTTP, the MCP server accepts only loopback `Host` headers and
  refuses any request with an `Origin` header. Don't weaken either check.
- Every loop that can run forever has a way to stop. Async code takes a
  `CancellationToken`; the engine stops when its `EngineHandle` is
  dropped.
- Never hold the session lock across an `.await`.
- Anything that can grow has a limit, like the 256-snapshot undo history.
  If something could grow without bound, cap it and test the cap, or
  explain in the PR why it can't grow.
- Public items get a short doc comment.

## Checks

Run all four before you say a change is done:

```sh
cargo fmt
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
```

- CI (`.github/workflows/ci.yml`) runs these on every pull request and
  on `main`, and also lints with default features and with each
  `treason-io` backend alone. If you change feature-gated code, run those
  lints too.
- If a check fails, read the whole error, fix the cause rather than the
  symptom, and run it again.
- Building needs the ALSA development files (`libasound2-dev` on Debian
  and Ubuntu) and Rust 1.95 from rustup. If `cargo --version` shows an
  older release, a system Rust is shadowing rustup's: put `~/.cargo/bin`
  first on `PATH`. If you can't build, say so. Never report a check you
  didn't run.

## Tests

- Changes that aren't trivial come with tests. Aim them at logic errors,
  edge cases and broken invariants, not only the expected path, and add a
  regression test for every bug you fix.
- Unit-test pure logic. Timing code needs its boundaries tested: window
  edges, loop wrap-around, a note-off and a note-on on the same tick,
  tempo changes.
- Tests are deterministic: no sleeping and no real MIDI ports. Use
  `ManualClock`, `RecordingOutput`, and fakes of `Playback` and
  `MidiOutput`.
- Use table-driven tests where cases share a shape, and name each test
  for what it proves.

To try a change by hand, run `cargo run -- serve`, connect an MCP client
to `http://127.0.0.1:6434/mcp` (for Claude Code:
`claude mcp add --transport http treason http://127.0.0.1:6434/mcp`), and
watch treason's virtual port with `aseqdump`.

## Documentation

Update the docs in the same PR as the behavior they describe:

- ARCHITECTURE.md: the design, the module status table and next steps.
- README.md: requirements and usage.
- `treason.toml`: the example configuration documents every setting.
  Keep it in step with `crates/treason/src/config.rs`.
- Tool doc comments: they are the agent's documentation.

## Pull requests

- One purpose per PR, on its own branch off `main`, small enough to
  review in one sitting.
- The description says what changed, why, and how you tested it.
- Opening, updating and merging a PR posts to the team's Telegram Reviews
  topic, so push when the work is ready for review, not after every
  commit.
- Squash-merge once the checks pass and the review is done.
- Never commit secrets. The Telegram bot token lives in the repository's
  GitHub Actions secrets.
