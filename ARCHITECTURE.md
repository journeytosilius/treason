# Architecture

treason is a MIDI sequencer driven by an AI agent. The agent edits the
sequencer's project through tools, and the sequencer plays it as MIDI to
external gear: drum machines, synths and other sequencers. treason makes no
sound itself.

## Principles

- **Decoupled.** `treason-core` holds the shared vocabulary and the traits
  that connect the crates. Library crates depend on core, not on each other
  (the control layer also reads device profiles). The engine, the MIDI
  backends and the agent interface can each be replaced or used on their own.
- **Efficient.** The engine thread never waits for the rest of the program.
  It reads project snapshots without waiting, takes commands from a lock-free
  queue, and doesn't allocate in steady state.
- **Reusable.** The engine can render to a file with no hardware attached.
  The control layer can be driven by a CLI or a UI as easily as by MCP. Core
  is useful to any MIDI tool.

## Crates

| Crate | Responsibility | Depends on |
|---|---|---|
| `treason-core` | MIDI types, musical time, the project model, and the traits between crates (`MidiOutput`, `Playback`). No I/O, threads or async. | nothing of ours |
| `treason-devices` | Device profiles: the names each synth or drum machine gives its notes and controls, loaded from TOML. | core |
| `treason-engine` | Real-time playback: timebase, transport, rendering, note tracking, MIDI clock, the engine thread. | core |
| `treason-io` | MIDI backends that implement `MidiOutput`, each behind a Cargo feature. | core |
| `treason-control` | The application layer: validates edits, keeps undo history, publishes snapshots through `Playback`. | core, devices |
| `treason-mcp` | The MCP server: the agent's tools, their schemas, and the HTTP and stdio transports. | control, core, devices |
| `treason` | The binary and composition root: CLI, configuration, wiring. | all |

```text
                       treason (bin)
          ┌──────────────────┼───────────────────┐
          ▼                  ▼                   ▼
     treason-mcp       treason-engine       treason-io
          │                  │                   │
          ▼                  │                   │
   treason-control           │                   │
     │          │            │                   │
     │          ▼            │                   │
     │   treason-devices     │                   │
     ▼          ▼            ▼                   ▼
  ═══════════════════════ treason-core ═══════════════════════
```

The engine and the backends never see each other: the binary hands the
engine a `MidiOutput`. Likewise the control layer never sees the engine. It
talks to a `Playback`, which the engine's handle implements and tests replace
with a fake.

## Repository layout

```text
Cargo.toml             workspace: members, shared dependency versions, lints
rust-toolchain.toml    Rust 1.95
treason.toml           example configuration, read by `treason serve`
devices/               device profiles (TOML)
crates/<crate>/src/    one directory per crate, modules below
```

Unwritten code is a `todo!()` stub with its final signature and docs, so the
seams between modules are fixed and compile-checked before the logic exists.

| Crate | Module | Contents | Status |
|---|---|---|---|
| core | `midi` | `MidiMessage` and its encoding, `U7`, `U14`, `Channel` | done |
| core | `time` | `Ticks` (960 PPQN), `Bpm`, `TimeSignature` | done |
| core | `model` | `Project`, `Track`, `Clip`, `ClipEvent` | done |
| core | `output` | `MidiOutput`, `TimedMessage`, `Delivery` | done |
| core | `playback` | `Playback`, `TransportCommand`, `PlaybackStatus` | done |
| devices | `profile`, `registry` | `DeviceProfile`, `DeviceRegistry` | done |
| engine | `link` | `EngineHandle` and `EngineLink`: snapshot pointer, command queue, status, returned snapshots | done |
| engine | `timebase` | `Timebase`: ticks to time and back | done |
| engine | `notes` | `ActiveNotes`: sounding-note bitsets | done |
| engine | `clock` | `Clock`, `SystemClock` (spin_sleep), `ManualClock` | done |
| engine | `transport` | `Transport`: MIDI transport semantics | done |
| engine | `render` | `render` (clip events in a window), `clock_pulses` | done |
| engine | `runtime` | `Engine`, `EngineConfig`, `spawn`, real-time priority | done |
| io | `memory` | `RecordingOutput` | done |
| io | `midir_output` (feature `midir`, default) | `MidirOutput`: ports, virtual ports, port listing | done |
| io | `alsa_seq` (feature `alsa-seq`) | `AlsaSeqOutput`: kernel-scheduled queue | stub |
| io | `smf` (feature `smf`) | `write`: Standard MIDI Files | stub |
| control | `edit`, `session` | `Edit`, `Session`: validation, undo and redo, publishing | done |
| mcp | `server` | `TreasonServer`: the tools | `set_notes` is a placeholder |
| mcp | `dto` | tool inputs and outputs | done |
| mcp | `notation` | step strings and pitch names | pitch names are a stub |
| mcp | `pattern` | step patterns to clips and back | done |
| mcp | `serve` | `serve_http`, `serve_stdio` | done |
| treason | `main`, `config`, `serve` | subcommands `serve`, `ports`, `devices`; `treason.toml` | `serve` doesn't start the engine yet |

## Runtime

```text
agent (Claude Code, Claude Desktop, any MCP client)
  │  MCP over HTTP on 127.0.0.1, or stdio
  ▼
treason-mcp        tool call → Edit or TransportCommand
  ▼
Session            validate, record undo, publish
  │  Playback
  ▼
EngineHandle ─┬─ ArcSwap<Project>    publish: one atomic pointer swap
              ├─ rtrb ring buffer    play, stop, continue, locate, panic
              └─ atomics ◄────────── status: playing, position
═════════════════ engine thread, real-time priority ═════════════════
  read snapshot → render the lookahead window → wait until due → send
  ▼
MidiOutput (treason-io) → MIDI ports → drum machines, synths, sequencers
```

`treason serve` is a long-lived process that owns the engine and serves MCP
over streamable HTTP, at `http://127.0.0.1:6434/mcp` by default. The music
keeps playing when an agent session ends, and several agents can connect at
once. `treason serve --stdio` serves a single agent over stdin and stdout.

The HTTP server accepts only loopback `Host` headers, which stops DNS
rebinding, and refuses requests that carry an `Origin` header, so a web page
can't drive the sequencer through the browser. MCP clients send no `Origin`.

Shutdown runs back down the chain: Ctrl-C stops the MCP server, which drops
the session and its `EngineHandle`. The engine sees the handle go, ends every
sounding note and returns.

## Timing

- Positions are whole ticks at 960 PPQN: 40 ticks per MIDI clock pulse, 240
  per 16th note (the Song Position Pointer unit), and exact triplets.
- The time of tick *n* is computed from an anchor, the last start, locate or
  tempo change: `anchor_time + (n − anchor_tick) × 60 s / (bpm × 960)`.
  Nothing accumulates, so there is no drift. `Timebase` does this in integer
  nanoseconds with tempo in thousandths of a BPM, so ticks and times
  round-trip exactly.
- The engine renders a short lookahead window, on the order of 10 to 50 ms,
  tuned per backend.
- `Delivery::Immediate` backends (midir) send when called, so the engine
  sleeps until each message is due, spinning for the last stretch.
  `Delivery::Scheduled` backends (an ALSA sequencer queue) take timestamped
  messages early, and the kernel sends them on time. `MidiOutput::cancel_from`
  retracts undelivered work, including flushed events, on stop, restart,
  locate, panic and shutdown. The engine keeps delivered note state separate
  from queued work so cancellation still ends every sounding note. It
  samples time again after cancellation and conservatively keeps note-ons
  that could have raced it; a cancelled note-off never proves a note ended.
  Its preallocated queue mirror holds a render batch plus a switch's note-offs;
  excess events stay in the render buffer until queue space becomes free.
- Each port has a latency offset. Messages for a slower device are due
  earlier.
- Tempo changes and transport commands apply at once. Other edits, such
  as a new clip, a mute or a move to another port, wait for the next bar
  line. There the notes still sounding from tracks that changed end; a
  track that only got a new name or device plays on. While stopped, edits
  apply at once. The boundary comes from the playhead, even if lookahead has
  already rendered or submitted that bar. Future work there is replaced,
  and the previous snapshot stays available until delivery, so another edit
  or undo can still replace the same upcoming bar.
- A tempo change anchors the current whole-tick position at the current
  time. Due events keep their old times; future rendered and submitted
  events are rebuilt using the new tempo, including any pending bar switch.
- Play starts from the top, and restarts there while playing. Continue
  resumes where playback stopped, and Stop keeps the position.
- At the same tick, note-offs go out first, then bank selects, program
  changes, other control changes and note-ons. A retriggered note isn't cut
  short, a program change gets the bank selected with it, and a note plays
  with the program and controller values set on its tick. Within each
  group, events keep their order, by track and then as written in the
  clip, so RPN sequences and repeated controllers arrive as written.
- treason is the clock master: a MIDI clock pulse every 40 ticks while
  playing, plus Start, Stop, Continue and Song Position Pointer, on the ports
  configured with `clock = true`. Following an external clock is planned.

## Engine thread rules

1. No locks, and no allocating or freeing in steady state. Buffers are
   allocated up front. Old snapshots are freed on the control side: the undo
   history keeps recent ones alive, and the engine hands back any it still
   holds.
2. No blocking except on the MIDI output.
3. Communication only through the `EngineLink`: `ArcSwap`, `rtrb` and atomics.
4. No stuck notes. `ActiveNotes`, a 256-byte bitset per port, supplies
   note-offs on stop, panic, clip changes and shutdown.

## Project model

- A `Project` is an immutable snapshot. Tracks sit behind `Arc`, so an edit
  copies a vector of pointers plus the one track that changed, and undo is a
  list of old snapshots.
- A `Track` sends to one `OutputTarget` (port and channel) and loops one
  `Clip`.
- A `Clip` has its own length, so clips of different lengths give polymeter.
  It holds note, control-change and program-change events sorted by start
  tick. Micro-timing is just where an event sits. A note can run past the
  loop point into the next pass, but can't be longer than the loop.
- Planned event fields: probability, ratchets, conditions (every N loops,
  fills) and per-step control values.
- The engine sees only resolved MIDI. Names such as "kick" or "cutoff" live
  in device profiles and are resolved by the control and MCP layers.

## Device profiles

TOML files in `devices/`. Each gives a device's default channel, its voices
(a note per sound), aliases (`bd = "kick"`) and parameters (CC numbers; NRPN
is planned). Names need a letter, because tools read a bare number as a MIDI
note. `devices/gm-drums.toml` covers the General MIDI drum map.
Profiles for real gear can be generated from the MIDI Guide dataset
(github.com/pencilresearch/midi, CC-BY-SA-4.0).

## Agent interface

The tools are designed for the model rather than mirroring the internal
model: voice names instead of note numbers, step strings such as
`"x...x...x..x.x.."`, channels 1–16, positions in bars and beats.
`treason-mcp` converts between the two, so either side can change without
breaking the other.

| Tools | |
|---|---|
| `get_project`, `status`, `list_devices`, `list_ports` | read only |
| `add_track`, `edit_track`, `remove_track`, `set_muted` | tracks |
| `set_steps`, `set_notes` | clips (`set_notes` is a placeholder until pitch names are written) |
| `set_tempo`, `set_time_signature` | project |
| `play`, `stop`, `panic` | transport |
| `undo`, `redo` | history |

Tool descriptions and parameter schemas come from the Rust doc comments
(through schemars), so the code documents the agent's interface. A rejected
call comes back as a tool error whose message says how to fix it, such as
"there is no port `x`; see list_ports", so the agent can correct itself.

`get_project` shows a clip that fits a step grid as step strings, in the
form `set_steps` takes, so the agent reads back patterns the way it wrote
them.

## Configuration

`treason.toml`, read from the working directory or given with `--config`.
Every setting has a default: the device directory, the MCP address, the
engine's lookahead and real-time priority, and the output ports. Each port
has a name that tracks refer to and either connects to an existing port by
name or is created as a virtual port. It can also set a latency offset and
whether the port gets MIDI clock. The example in the repository root
documents each setting.

## Dependencies

All support Rust 1.95 (checked on crates.io on 2026-10-16).

| Crate | Version | Where | Purpose |
|---|---|---|---|
| serde, toml, thiserror | 1.0, 0.9, 2.0 | most crates | data formats, errors |
| arc-swap, rtrb | 1.9, 0.4 | engine | snapshot hand-off, command queue |
| spin_sleep, thread-priority | 1.3, 3.1 | engine | accurate waits, real-time priority |
| midir | 0.11 | io, feature `midir` | portable MIDI output and virtual ports; needs `libasound2-dev` |
| alsa | 0.12 | io, feature `alsa-seq` | kernel-scheduled output on Linux |
| midly | 0.5 | io, feature `smf` | MIDI file export |
| rmcp, schemars | 3.5, 1.2 | mcp | MCP server and tool schemas (rmcp needs Rust 1.88 or later) |
| tokio, tokio-util, axum | 1.53, 0.7, 0.8 | mcp, bin | async runtime, HTTP server |
| clap, anyhow | 4.6, 1.0 | bin | CLI, error reporting |
| tracing, tracing-subscriber | 0.1, 0.3 | engine, mcp, bin | logging, to stderr |
| rusty_link | 0.4 | engine (later) | Ableton Link |

Backends that need system libraries sit behind features in `treason-io`, so
nothing else has to link them.

## Conventions

- Rust 1.95, pinned in `rust-toolchain.toml`. `rust-version = "1.95"` makes
  Cargo's resolver pick dependency versions that support it. Edition 2024.
- `unsafe_code` is denied across the workspace.
- Checks: `cargo fmt --check`,
  `cargo clippy --workspace --all-targets --all-features -- -D warnings`,
  `cargo test --workspace --all-features` and
  `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features`.
  CI runs them on every pull request and on `main`, and also lints with
  default features and with each `treason-io` backend alone.
- Channels are 1–16 wherever people or agents see them (files, tools) and
  0–15 internally. `Channel` handles the conversion.
- Stubs carry `#[expect(unused_variables, reason = "not implemented yet")]`
  (or `dead_code`) instead of `allow`. Once the code is written the
  expectation goes unfulfilled, and the compiler asks for the attribute to
  be removed.

## Next steps

One pull request each, in this order. Each marks its step done here. The
first milestone ends when an agent writes a drum pattern and external
gear plays it:

1. CI: formatting, lints, tests and docs on every pull request. Done.
2. Step patterns: step strings in `notation` and the `set_steps` tool,
   with `get_project` showing patterns as step strings. Also an
   `edit_track` tool, to rename a track or move it to another port,
   channel or device without losing its clip. Done.
3. Engine core: `Transport::apply` and `render`. Done.
4. Engine loop: `Engine::run`, tested with `ManualClock` and
   `RecordingOutput`. Done.
5. `treason serve` starts the engine, so `play` plays.

The second milestone drives other gear:

6. Clock master: MIDI clock, Start, Stop, Continue and Song Position
   Pointer on the ports with `clock = true`, and per-port latency.
7. Melodic parts: pitch names in `notation` and the `set_notes` tool.
8. The ALSA sequencer queue backend, for kernel-timed delivery.

Later: MIDI file export, following an external clock (which needs MIDI
input), and probability, ratchets and conditions on events.
