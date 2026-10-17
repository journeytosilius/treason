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
| `treason-control` | The application layer: validates edits, keeps undo history, publishes snapshots through `Playback`, saves and loads projects. | core, devices |
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
| engine | `timebase` | `Timebase`: ticks to when they're heard and back, across tempo changes | done |
| engine | `notes` | `ActiveNotes`: sounding-note bitsets | done |
| engine | `clock` | `Clock`, `SystemClock` (spin_sleep), `ManualClock` | done |
| engine | `transport` | `Transport`: MIDI transport semantics and the messages clock ports get | done |
| engine | `render` | `render` (clip events and clock pulses in a window), `clock_pulses` | done |
| engine | `runtime` | `Engine`, `EngineConfig`, `spawn`, per-port latency, real-time priority | done |
| io | `memory` | `RecordingOutput` | done |
| io | `midir_output` (feature `midir`, default) | `MidirOutput`: ports, virtual ports, port listing | done |
| io | `alsa_seq` (feature `alsa-seq`) | `AlsaSeqOutput`: a kernel-scheduled queue that knows exactly what it sent | done |
| io | `smf` (feature `smf`) | `write`: Standard MIDI Files | stub |
| control | `edit`, `session` | `Edit`, `Session`: validation, undo and redo, publishing | done |
| control | `file`, `store` | project files; `ProjectStore`: saving and loading by name | done |
| mcp | `server` | `TreasonServer`: the tools | done |
| mcp | `dto` | tool inputs and outputs | done |
| mcp | `notation` | step strings and pitch names | done |
| mcp | `pattern` | step patterns to clips and back | done |
| mcp | `notes` | lists of notes in beats to clips | done |
| mcp | `serve` | `serve_http`, `serve_stdio` | done |
| treason | `main`, `config`, `serve` | subcommands `serve`, `ports`, `devices`; `treason.toml`; picks the backend | done |

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

Shutdown runs back down the chain. Ctrl-C, SIGTERM or, with `--stdio`, the
agent closing stdin stops the MCP server. HTTP requests get up to one second
to drain; an unfinished request body cannot delay shutdown beyond that.
Shutting down the async runtime then drops the remaining connection tasks,
the session and its `EngineHandle`. The engine sees the handle go,
ends every sounding note and returns, and `treason serve` waits for it before
exiting. The runtime gets a second to shut down. That is because a pending
read from stdin can't be cancelled, and would otherwise hold the process
until the next line of input. If the MIDI output fails, the engine logs why
and stops, and the transport tools report that it isn't running.

## Timing

- Positions are whole ticks at 960 PPQN: 40 ticks per MIDI clock pulse, 240
  per 16th note (the Song Position Pointer unit), and exact triplets.
- The time tick *n* is heard is computed from an anchor, the last start,
  locate or tempo change: `anchor_time + (n − anchor_tick) × 60 s / (bpm × 960)`.
  The ticks up to a tempo change keep the anchor before it. Nothing
  accumulates, so there is no drift. `Timebase` does this in integer
  nanoseconds with tempo in thousandths of a BPM, so ticks and times
  round-trip exactly.
- The engine renders a short lookahead window, on the order of 10 to 50 ms,
  tuned per backend. Each port renders into its own preallocated 4096-event
  buffer, which also holds any retained events. Oversized windows resume in
  send order, even within a tick, without losing events or growing the
  buffer.
- `Delivery::Immediate` backends (midir) send when called, so the engine
  sleeps until each message is due, spinning for the last stretch.
  `Delivery::Scheduled` backends (an ALSA sequencer queue) take timestamped
  messages early, and the kernel sends them on time: on the sequencer
  timer's first tick at or after each message's time, so within a
  millisecond. That is coarser than midir's on an idle machine, but it
  holds when the engine thread can't run on time. The queue also sends each
  message to a private port of the same client, so the backend sees exactly
  which ones went out.
  `MidiOutput::cancel_from`
  retracts undelivered work, including flushed events, on stop, restart,
  locate, panic and shutdown. The engine keeps delivered note state separate
  from queued work so cancellation still ends every sounding note. It
  samples time again after cancellation and conservatively keeps note-ons
  that could have raced it; a cancelled note-off never proves a note ended.
  Outputs report exactly which queued messages were withdrawn. The engine
  retains each port's delivered clock position, including pulses delivered
  during cancellation, so rebuilding future work never repeats those pulses.
  When submission crosses queued deadlines, it reconciles older deliveries
  before recording the newly submitted message as delivered.
  Its preallocated queue mirror holds a render batch plus a switch's note-offs;
  excess events stay in the render buffer until queue space becomes free.
- Each port has a latency: how long its device takes to respond. The
  timebase says when each tick is heard, and each port sends its messages
  that much earlier, so all the gear sounds together. Each port renders and
  sends at its own pace. Playback starts once the port with the most latency
  can send the first tick, so starting takes that long. Changes take effect
  after the last tick that port may have sent, and the ports with less
  latency still send the ticks before it as they were. Each port ends the
  changed tracks' notes and starts the new bar when it reaches the bar line.
  An edit that arrives while a port already plays that bar waits until every
  port does, and a tempo change waits until the previous one is heard. Panic
  ends the notes on every port at once, and each port carries on right
  after the last tick it sent. Cancellation before a port's first send time
  resumes from its start tick, including its first clock pulse.
- Tempo changes and transport commands apply at once. Other edits, such
  as a new clip, a mute or a move to another port, wait for the next bar
  line. There the notes still sounding from tracks that changed end; a
  track that only got a new name or device plays on. While stopped, edits
  apply at once. The boundary comes from the playhead, even if lookahead has
  already rendered or submitted that bar. Future work there is replaced,
  and the previous snapshot stays available until delivery, so another edit
  or undo can still replace the same upcoming bar.
  A switch completes only after every port has reached its send boundary;
  submitting every port's cleanup early does not retire the old snapshot.
  Rendered events and scheduled submissions retain their track IDs. A fixed
  ownership table per port selects only the changed tracks' notes, preserving
  held notes from other tracks sharing the channel. When tracks retrigger
  the same MIDI key, the most recent note-on owns it; an older track's
  note-off cannot end that retrigger.
- A tempo change applies after the last whole tick any port may have sent,
  which lasts until the current time plus the most latency. Earlier ticks
  keep their times, so ports with less latency still send them on time;
  later rendered and submitted events are rebuilt using the new tempo,
  including any pending bar switch.
- Play starts from the top, and restarts there while playing. Continue
  resumes where playback stopped, and Stop keeps the position.
- At the same tick, note-offs go out first, then bank selects, program
  changes, other control changes and note-ons. A retriggered note isn't cut
  short, a program change gets the bank selected with it, and a note plays
  with the program and controller values set on its tick. Within each
  group, events keep their order, by track and then as written in the
  clip, so RPN sequences and repeated controllers arrive as written.
- treason is the clock master for the ports configured with
  `clock = true`. While playing, they get a MIDI clock pulse every 40 ticks,
  first among the messages on its tick. Play sends them Start. The engine
  flushes Start or Continue and sends each clock port its first pulse at
  least 1 ms later, so the receivers can prepare. The clip events start with
  the pulses, on every port.
  Continue and Locate send a Song Position Pointer and Continue: the receivers
  resume at the pointer's 16th, the first at or after the position, on the
  next pulse.
  So pulses resume there too, and none come before it. Since receivers
  ignore Song Position Pointer while playing, restarting or locating while
  playing sends Stop first. Stop sends Stop after the note-offs, and so does
  shutdown while playing. Locate while stopped sends only a Song Position
  Pointer. Panic sends nothing, and the clock runs on. Song Position Pointer
  counts 16ths in 14 bits, so it wraps after 1024 bars of 4/4. Following an
  external clock is planned.

## Engine thread rules

1. No locks, and no allocating or freeing in steady state. Buffers are
   allocated up front. Old snapshots are freed on the control side: the undo
   history keeps recent ones alive, and the engine hands back any it still
   holds.
2. No blocking except on the MIDI output.
3. Communication only through the `EngineLink`: `ArcSwap`, `rtrb` and atomics.
4. No stuck notes. `ActiveNotes`, a 256-byte bitset per port, supplies
   note-offs on stop, panic, clip changes and shutdown. The engine adds
   fixed ownership tables (one track ID per MIDI key) to its projected and
   delivered sets, allocated before playback, for selective track cleanup.

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
model: voice names and pitch names such as `C4` instead of note numbers,
step strings such as `"x...x...x..x.x.."`, channels 1–16, positions in bars
and beats.
`treason-mcp` converts between the two, so either side can change without
breaking the other.

| Tools | |
|---|---|
| `get_project`, `status`, `list_devices`, `list_ports`, `list_projects` | read only |
| `add_track`, `edit_track`, `remove_track`, `set_muted` | tracks |
| `set_steps`, `set_notes` | clips |
| `set_tempo`, `set_time_signature` | project |
| `play`, `stop`, `panic` | transport |
| `undo`, `redo` | history |
| `save_project`, `load_project` | saved projects |

Tool descriptions and parameter schemas come from the Rust doc comments
(through schemars), so the code documents the agent's interface. A rejected
call comes back as a tool error whose message says how to fix it, such as
"there is no port `x`; see list_ports", so the agent can correct itself.

`get_project` shows a clip that fits a step grid as step strings, in the
form `set_steps` takes, and any other clip as a list of notes, in the form
`set_notes` takes. So the agent reads back what it wrote, in the same terms.
Notes go by the device's voice names where it has them and by pitch
otherwise, with sharps: C4 is middle C, note 60. If that pitch name is a
device voice or alias for another note, readback uses the decimal MIDI
number instead, so writing the notes back preserves their pitches.

## Saved projects

`save_project` writes the project to `<projects>/<name>.toml`, and
`load_project` reads it back exactly: the tempo, the time signature, and
every track with its id, name, device, port, channel, mute and clip.
`treason serve --project <name>` starts with one, and `treason projects`
lists them.

- The format is the control layer's own, not the core model's serde form,
  so the model can change without breaking saved projects. Each file
  starts with `format = 1`, and a file in a format this version doesn't
  read is refused as such.
- Tracks name their port, since a port's number is its place in
  `treason.toml`. Times are whole ticks, and events go one to a line, in
  the clip's order, so events on the same tick keep their order.
- Loading checks what an edit would: every port and device exists, track
  ids are unique and clips are valid. A file that fails changes nothing.
- Saving writes a temporary file, flushes it to disk and renames it over
  the old one, so a failed save leaves the old project as it was. A
  project file holds at most 16 MiB, and saving refuses a bigger project,
  so every saved project loads.
- Names hold letters, digits, spaces, dashes and underscores, so they can't
  reach outside the directory or hide a file.

The session remembers the name the project was last saved as or loaded
from, and the snapshot it saved then. Any other snapshot has unsaved
changes, so undoing back to the save leaves none. Saving without a name
saves under the project's own name, and saving over another project needs
`overwrite`. Loading replaces the project and clears the undo history,
because stepping back past a load would leave the session named after a
project it no longer holds, and the next save would overwrite that
project. So loading refuses to drop unsaved changes unless told to. While
playing, a load applies like any edit: the tempo at once, the tracks at
the next bar.

## Configuration

`treason.toml`, read from the working directory or given with `--config`.
Every setting has a default: the device directory, the directory for
saved projects, the MCP address, the engine's lookahead and real-time
priority, the backend, and the output ports. The backend is midir unless `engine.backend` names the ALSA
sequencer queue (`alsa-seq`, Linux only), and the binary turns on
`treason-io`'s `alsa-seq` feature on Linux. Each port
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
5. `treason serve` starts the engine, so `play` plays. Done.

The second milestone drives other gear:

6. Clock master: MIDI clock, Start, Stop, Continue and Song Position
   Pointer on the ports with `clock = true`. Done.
7. Per-port latency: messages for slower devices go out earlier, so every
   port sounds in time. Done.
8. Melodic parts: pitch names in `notation` and the `set_notes` tool.
   Done.
9. The ALSA sequencer queue backend, for kernel-timed delivery. Done.

The third milestone keeps the work:

10. Saved projects: `save_project`, `load_project` and `list_projects`,
    `treason serve --project` and `treason projects`. Done.
11. MIDI file export: `smf::write`, so a project opens in a DAW.

Later: following an external clock (which needs MIDI input), and
probability, ratchets and conditions on events.
