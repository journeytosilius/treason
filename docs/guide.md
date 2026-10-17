# Driving treason: a guide for agents

treason is a MIDI sequencer that an AI agent drives through MCP tools. It
plays drum machines, synths and other sequencers over MIDI, and makes no
sound itself. This guide holds everything you need to make music with it:
how a project works, what each tool is for, the notations, the limits, what
the errors mean, and how to connect gear. The `get_guide` tool returns it,
and the repository keeps it as `docs/guide.md`. [tools.md](tools.md) lists
every field of every tool, as the tools' own schemas do.

The examples show each call as the tool's name and then its arguments in
JSON. treason's tests run them in order, as one session, so each works as
written.

## Quick start

treason keeps running between agent sessions, so a project may already be
playing when you connect: `status` says, and `get_project` shows it. Build
on it unless the user asks for something new. From an empty project:

```mcp
list_devices {}
list_ports {}
add_track {"name": "drums", "device": "gm-drums"}
set_steps {"track": 1, "steps": {
  "kick":       "x...x...x...x...",
  "snare":      "....x.......x...",
  "closed_hat": "..x...x...x...x."
}}
add_track {"name": "bass", "channel": 2}
set_notes {"track": 2, "notes": [
  {"note": "C2", "start": 0, "length": 0.5},
  {"note": "C2", "start": 1.5, "length": 0.25},
  {"note": "Eb2", "start": 2, "length": 0.5},
  {"note": "G1", "start": 3, "length": 0.75}
]}
set_tempo {"bpm": 122}
play {}
save_project {"name": "first groove"}
```

`add_track` returns the new track with its `id`, which the other tools
take; ids count from 1. The drum track takes its device's channel, 10, and
both tracks play on the first port, since neither names one. Each step
string is one bar of sixteenths, and the bass notes end in the first bar,
so both loops last one bar. Nothing sounds unless the port is connected to
gear: see [Ports and gear](#ports-and-gear).

## How a project works

- **One project at a time**, held in memory until `save_project` writes
  it: a tempo, a time signature and tracks. Every agent connected to the
  same treason shares it, so call `get_project` before you change a track
  you didn't just write.
- **A track** plays one loop on one MIDI port and channel (1-16). It may
  name a device profile, so its notes and controls can go by the device's
  names, and it can be muted.
- **A loop** holds notes and control changes (MIDI CC), and repeats until
  `stop`. Each track's loop has its own length, `loop_beats`. Loops count
  from the start of bar 1, so a two-bar loop plays its first bar on bars 1,
  3, 5 and so on, even if you set it during bar 4. Loops of different
  lengths give polymeter: a 3-beat loop against a 4-beat one lines up again
  every 12 beats.
- **Beats are quarter notes** in every tool input, whatever the time
  signature: `start`, `length`, `at`, `loop_beats` and `repeat_beats` count
  quarter notes, and `steps_per_beat` divides one. 0 is the start of the
  loop, and 1.5 the offbeat after the second beat. A bar of 4/4 is 4 beats, of 6/8 3
  beats, and of 7/8 3.5 beats. Times round to 1/960 of a beat.
- **Bars count from 1.** `status` shows the bar and the beat in the time
  signature's own beats, so in 6/8 its beat goes from 1 to 6.
- **Edits apply while playing.** `set_tempo` applies at once. Every other
  edit waits for the next bar line, where the notes of the tracks that
  changed end and the new version starts. Tracks that didn't change play on.
  While stopped, edits apply at once.
- **Every edit can be undone**, up to 256 of them. Playing, stopping,
  saving and exporting aren't edits.

## Names for notes and controls

`list_devices` shows each device profile: the `id` that `add_track` takes,
the channel the device listens on, its voices (a MIDI note each, such as
`kick` = 36), their aliases (`bd` for `kick`) and its params (a CC number
each, such as `cutoff` = 74).

treason comes with `gm-drums`, the General MIDI drum map, on channel 10. Its
voices include `kick`, `acoustic_kick`, `snare`, `electric_snare`, `clap`,
`side_stick`, `closed_hat`, `pedal_hat`, `open_hat`, `low_floor_tom`,
`high_floor_tom`, `low_tom`, `low_mid_tom`, `high_mid_tom`, `high_tom`,
`crash`, `crash_2`, `splash`, `china`, `ride`, `ride_2`, `ride_bell`,
`tambourine`, `cowbell`, `maracas`, `cabasa`, `claves`, bongos, congas,
timbales, agogos, wood blocks, cuicas and triangles, with the aliases `bd`,
`sd`, `rs`, `rimshot`, `cp`, `hh`, `ch`, `oh`, `lt`, `mt`, `ht`, `cy`, `rd`
and `cb`. A profile for other gear is a small file: see
[Device profiles](#device-profiles).

A note goes by, in this order:

1. a MIDI note number, 0-127: a JSON number, or digits in a string such
   as `"36"`;
2. a voice or alias of the track's device, such as `kick` or `bd`;
3. a pitch (`set_notes` only): a letter A-G, an optional sharp (`#` or `♯`)
   or flat (`b` or `♭`), then an octave from -1 to 9. `C4` is middle C, MIDI
   note 60. `C-1` is note 0 and `G9` note 127.

A control (`set_controls`) goes by, in this order:

1. a controller number in a string, such as `"74"`: 0-119. 120-127 are
   channel mode messages, such as All Notes Off, which treason sends
   itself, so they are refused;
2. a param of the track's device;
3. a standard name, with its number: `modulation` 1 (the mod wheel),
   `breath` 2, `foot` 4, `portamento_time` 5, `volume` 7, `balance` 8,
   `pan` 10, `expression` 11, `sustain` 64, `portamento` 65, `sostenuto`
   66, `soft_pedal` 67, `legato` 68, `resonance` 71, `release` 72,
   `attack` 73, `cutoff` 74, `decay` 75, `vibrato_rate` 76,
   `vibrato_depth` 77, `vibrato_delay` 78, `reverb` 91, `tremolo` 92,
   `chorus` 93, `detune` 94 and `phaser` 95. Synths differ in what they do
   with 71-95: the device's manual says.

`get_project` reads names back the same way: notes by voice, else by pitch
with sharps, and controls by the device's param, else the standard name,
else the number. A pitch whose name the device uses for a different note
reads back as a number, so writing it back keeps the note.

## Drums: set_steps

`set_steps` writes a track's notes as one step string per voice:

- `x` is a hit at velocity 100, `X` an accent at 127, and `.` or `-` a rest.
  Spaces and `|` are ignored, so `"x... x... | x... x..."` is fine.
- Each hit lasts one step. `steps_per_beat` sets the step: 4, the default,
  makes sixteenths, 2 eighths, 3 eighth-note triplets and 6 sixteenth-note
  triplets. It takes 1, 2, 3, 4, 6, 8, 12, 16, 24 or 32.
- The loop is as long as the longest string. A shorter string repeats to
  fill it, so its length must divide the loop's.
- Keys are the device's voices or aliases, or MIDI note numbers. A track
  without a device takes only numbers. Pitch names such as `C4` need
  `set_notes`.
- It replaces all the track's notes, and keeps its controls. Velocities
  other than 100 and 127 need `set_notes`.

A two-bar groove: a crash on the one, with a bar of kick and snare and a
beat of hats repeating inside it:

```mcp
set_steps {"track": 1, "steps": {
  "kick":       "X..x..x...X..x..",
  "snare":      "....X.......X..x",
  "closed_hat": "x.x.",
  "crash":      "X..............................."
}}
```

A cowbell that loops every 3 beats, across the drums' bar lines, and then
eighth-note triplets in its place:

```mcp
add_track {"name": "percussion", "device": "gm-drums"}
set_steps {"track": 3, "steps": {"cowbell": "x..x..x..x.."}}
set_steps {"track": 3, "steps": {"maracas": "x.xx.xx.xx.x"}, "steps_per_beat": 3}
```

`get_project` shows a loop as steps, with a space between beats, when
every note is one step long, on a step, at velocity 100 or 127.

## Melodies, basslines and chords: set_notes

`set_notes` writes a track's notes as a list. Each note has a `note` (a
name or number, as above), a `start` and a `length` in beats, and a
`velocity`, 1-127, 100 if left out.

- Notes that start together make a chord. Two notes of the same pitch
  can't overlap: end the first before the second starts.
- `loop_beats` sets the loop. Without it, the loop runs to the end of the
  last note, rounded up to a whole bar. Every note must start inside the
  loop, and none may be longer than it. A note may run past the loop's end
  into the next pass.
- A call holds at most 1024 notes.
- It replaces all the track's notes, and keeps its controls.

Four bars of chords, C minor, A flat, B flat, G:

```mcp
add_track {"name": "keys", "channel": 3}
set_notes {"track": 4, "loop_beats": 16, "notes": [
  {"note": "C4", "start": 0, "length": 3.5}, {"note": "Eb4", "start": 0, "length": 3.5},
  {"note": "G4", "start": 0, "length": 3.5},
  {"note": "Ab3", "start": 4, "length": 3.5}, {"note": "C4", "start": 4, "length": 3.5},
  {"note": "Eb4", "start": 4, "length": 3.5},
  {"note": "Bb3", "start": 8, "length": 3.5}, {"note": "D4", "start": 8, "length": 3.5},
  {"note": "F4", "start": 8, "length": 3.5},
  {"note": "G3", "start": 12, "length": 3.5}, {"note": "B3", "start": 12, "length": 3.5},
  {"note": "D4", "start": 12, "length": 3.5}
]}
```

Notes can use a device's voices too, at any time and velocity. Swung
hats, with every second eighth late and softer:

```mcp
set_notes {"track": 3, "loop_beats": 2, "notes": [
  {"note": "closed_hat", "start": 0, "length": 0.25},
  {"note": "closed_hat", "start": 0.58, "length": 0.25, "velocity": 70},
  {"note": "closed_hat", "start": 1, "length": 0.25},
  {"note": "closed_hat", "start": 1.58, "length": 0.25, "velocity": 70}
]}
```

## Controls: set_controls

`set_controls` moves a track's controls, such as a mod wheel, a filter's
cutoff or a volume, by sending control changes through the loop. A track
can send controls with no notes at all: modulation for a synth that plays
its own notes, for example.

- `controls` maps each control's name to its `points`: a `value`, 0-127,
  at a time `at`, in beats. Without `ramp` the value jumps there.
  `"ramp": "linear"` ramps evenly from the point before, and
  `"ramp": "smooth"` eases out and in, along half a sine wave, so smooth
  ramps between a low and a high make a sine-wave LFO. The first point
  can't ramp.
- A ramp sends a new value at most every 1/32 of a beat, and only when it
  changes, which keeps a slow DIN MIDI port clear.
- Points may be in any order. Jumps at the same time are sent in list
  order. A ramp must end later than the point before it.
- `repeat_beats` repeats the points through the loop: 1 cycles an LFO once
  a beat. A repeat that doesn't divide the loop starts again with it.
- A ramp may end at the end of the loop, or of the repeat. A jump there
  is refused, since it would send nothing: the next pass starts again from
  the first point.
- The call replaces only the controls it names. The track's notes and its
  other controls stay, and a control with no points is removed.
- On a track with notes, the notes set the loop. On a track without,
  `loop_beats` sets it. Without that, the loop stays as it is, or on a
  track with nothing else, it runs to the last point or the end of the
  repeat, rounded up to a whole bar.
- A control holds at most 8192 points, and a loop at most 8192 control
  changes.

A four-bar mod-wheel swell under the chords, ending on the loop's end, and
an LFO on their cutoff, once every two beats:

```mcp
set_controls {"track": 4, "controls": {"modulation": {"points": [
  {"at": 0, "value": 0},
  {"at": 16, "value": 127, "ramp": "linear"}
]}}}
set_controls {"track": 4, "controls": {"cutoff": {"points": [
  {"at": 0, "value": 40},
  {"at": 1, "value": 100, "ramp": "smooth"},
  {"at": 2, "value": 40, "ramp": "smooth"}
], "repeat_beats": 2}}}
```

Modulation alone, for a synth that plays its own sequence. The track sends
nothing but control changes, here to a port named `synth`:

```mcp
add_track {"name": "synth mod", "port": "synth", "channel": 1}
set_controls {"track": 5, "loop_beats": 8, "controls": {"modulation": {"points": [
  {"at": 0, "value": 0},
  {"at": 4, "value": 90, "ramp": "smooth"},
  {"at": 8, "value": 0, "ramp": "smooth"}
]}}}
```

A volume pump on the bass that ducks on every beat, then takes the
cutoff's LFO away again:

```mcp
set_controls {"track": 2, "controls": {"volume": {"points": [
  {"at": 0, "value": 40},
  {"at": 0.75, "value": 110, "ramp": "smooth"}
], "repeat_beats": 1}}}
set_controls {"track": 4, "controls": {"cutoff": {"points": []}}}
```

`get_project` reads controls back as points in this form: a ramp wherever
one gives exactly the values sent, otherwise jumps, and a repeat as every
pass of it. `set_steps` and `set_notes` keep a track's control changes
where they are, cut off by a shorter loop. A repeat doesn't stretch into a
longer loop, so call `set_controls` again after lengthening one.

Sustain (64) and sostenuto (66) hold notes on until they are released.
`stop`, `panic`, muting, removing or changing a track, and the end of an
export release them. Pedals act on the whole channel, so tracks on the
same channel share them.

## Tracks

```mcp
edit_track {"track": 3, "name": "hats"}
set_muted {"track": 3, "muted": true}
set_muted {"track": 3, "muted": false}
remove_track {"track": 3}
undo {}
```

- `add_track` takes a `name`, and optionally a `device`, a `port` (the
  first by default) and a `channel` (the device's, else 1). The new track
  loops one empty bar.
- `edit_track` renames a track or moves it to another port, channel or
  device, keeping its loop. A new device renames nothing already written.
- `set_muted` silences a track and keeps its loop.
- `remove_track` deletes it. `undo` brings it back.

To clear a track's notes and keep its controls, write a pattern of rests
as long as the loop you want. Here, a bar of rests for the bass, then
`undo` to bring its notes back:

```mcp
set_steps {"track": 2, "steps": {"36": "................"}}
undo {}
```

## Playing

```mcp
play {"from_bar": 5}
status {}
stop {}
play {}
panic {}
```

- `play` starts from bar 1, or from `from_bar`, and restarts there if
  already playing. Every track loops until `stop`.
- `stop` ends every sounding note and keeps the position.
- `panic` ends every note on every port at once and releases the pedals,
  for stuck notes. Playback carries on.
- `status` says whether it's playing, the bar and beat, the tempo, the
  project's name and whether it has unsaved changes.
- `set_tempo` takes 20-400 BPM, and applies at once.
  `set_time_signature` takes 1-32 beats per bar and a beat of 1, 2, 4, 8,
  16 or 32. It changes the bar, and so where edits take effect and what
  `from_bar` counts; loops keep their length in beats:

```mcp
set_time_signature {"numerator": 6, "denominator": 8}
undo {}
```

Ports configured with `clock = true` also get MIDI clock, 24 pulses a
beat, and Start, Stop, Continue and Song Position Pointer, so drum machines
and sequencers there play along in time, from the same bar.

## Reading the project back

```mcp
get_project {}
```

`get_project` shows the tempo, the time signature and every track: its
id, name, port, channel, device, mute, `loop_beats`, how many events its
loop holds, and its notes, as steps (`pattern`) or as a list (`notes`),
and its `controls`, each in the form the matching tool takes. Every tool
that changes a track returns it in the same form, as it will play.

## Undo, saving and exporting

```mcp
set_tempo {"bpm": 126}
undo {}
redo {}
save_project {}
list_projects {}
export_midi {}
export_midi {"name": "first groove 8 bars", "bars": 8}
load_project {"name": "first groove"}
```

- `undo` and `redo` step through the last 256 edits.
- `save_project` writes the project, with its tempo, time signature and
  every track, under a name: up to 64 letters, digits, spaces, dashes and
  underscores, starting with a letter or digit. Without a name it saves
  under the project's own name, the one it was last saved as or loaded
  from. Saving over another project needs `"overwrite": true`.
- `load_project` replaces the project and clears the undo history. It
  refuses while there are unsaved changes, unless given
  `"discard_changes": true`. While playing, the loaded tempo applies at
  once and its tracks at the next bar. `list_projects` lists saved
  projects, newest first.
- `export_midi` writes a Standard MIDI File that a DAW opens, with a track
  for each unmuted track, the tempo and the time signature. By default it
  runs from bar 1 until every loop ends on the same bar line, so each
  pattern is complete; `bars` sets the length, at most 1000. Its `name`
  defaults to the project's, and replacing another file needs
  `"overwrite": true`. It replies with the file's path.

## Limits

| What | Limit |
|---|---|
| Tempo | 20-400 BPM |
| Time signature | 1-32 beats a bar, each 1, 2, 4, 8, 16 or 32 |
| Loop, start, length, `at`, `repeat_beats` | up to 1024 beats |
| Notes in a `set_notes` call | 1024 |
| Steps in a step string | 1024 |
| Points in a control | 8192 |
| Control changes in a loop | 8192 |
| Undo history | 256 edits |
| Project and export names | 64 characters |
| Saved project file | 16 MiB |
| Export | 1000 bars and 1,000,000 MIDI events |

## When a call fails

A refused call changes nothing, and its error says how to fix it, such as
"there is no track 9; see get_project". The usual causes:

- An unknown name. The error lists the device's voices or params;
  `list_devices` shows its aliases too. A track without a device takes
  only numbers, pitch names and standard control names: give it one with
  `edit_track`.
- A step string whose length doesn't divide the loop's.
- Two notes of the same pitch that overlap, or a note that starts outside
  the loop or is longer than it.
- `loop_beats` in `set_controls` on a track whose notes set its loop.
- A point that ramps first, ramps at the same time as the point before,
  or jumps at the end of the loop.
- `load_project` with unsaved changes, or a save or export over another
  file without `overwrite`.

## Ports and gear

`list_ports` names the MIDI output ports that `treason.toml` sets up; the
first is the default. Agents can't add ports. treason makes no sound, so
if the user hears nothing:

1. Check that it's playing (`status`) and the track isn't muted.
2. Check the port reaches the gear. A port without `connect` is virtual:
   on Linux, `aconnect -l` lists the connections, and
   `aconnect treason:0 <synth>:0` makes one. `aseqdump -p treason:0` shows
   what treason sends on its first port.
3. Check the track's channel is the one the gear listens on. Drum
   machines often use 10.

To hear treason without hardware, connect it to a software synth, such as
FluidSynth with a General MIDI sound font.

### treason.toml

`treason serve` reads `treason.toml` from the working directory, or the
file `--config` names. Every setting is optional; relative paths are from
the file. Edit it and restart treason to change ports.

```toml treason.toml
devices = "devices"    # device profiles, *.toml
projects = "projects"  # saved projects
exports = "exports"    # exported MIDI files

[mcp]
http = "127.0.0.1:6434"  # keep it on loopback

[engine]
lookahead_ms = 20     # how far ahead it renders, 1-1000
realtime = true       # ask for real-time priority
backend = "alsa-seq"  # "midir" (the default) or, on Linux, "alsa-seq"

[[ports]]
name = "drums"        # what tracks and list_ports call it
connect = "TR-8S"     # connect to the first port whose name contains this
latency_ms = 2.5      # the gear's delay, 0-1000: its messages go out earlier
clock = true          # send MIDI clock and transport

[[ports]]
name = "synth"        # no connect: a virtual port to connect yourself
```

`alsa-seq` hands each message to the kernel ahead of time, and keeps the
timing within a millisecond even on a busy computer.

### Device profiles

A profile names a device's notes and controls. It is a TOML file in the
devices directory, read when treason starts:

```toml device
id = "beatbox"              # what add_track takes
name = "Beatbox drum machine"
manufacturer = "Example"    # optional
default_channel = 10        # 1-16

[voices]                    # a MIDI note per sound
kick = 36
snare = 38
closed_hat = 42

[aliases]                   # other names for voices
bd = "kick"

[params]                    # named controls, with CC numbers from its manual
kick_decay = { cc = 23 }
snare_tune = { cc = 20 }
```

A synth's profile usually has only params. Names need a letter, since a
number means a MIDI note or controller. An alias must point to a voice and
can't share its name. A param's name wins over a standard name of the
same spelling. The MIDI Guide (github.com/pencilresearch/midi) lists the
CC numbers of many synths and drum machines.

### Running treason

```sh
treason serve                  # MCP over HTTP at http://127.0.0.1:6434/mcp
treason serve --stdio          # MCP over stdin and stdout, for one agent
treason serve --http ADDR      # another address
treason serve --project jam    # start with the saved project "jam"
treason ports                  # the MIDI output ports on this machine
treason devices                # the device profiles
treason projects               # the saved projects, newest first
treason export jam --bars 8    # projects/jam.toml as exports/jam.mid
```

From a checkout of the repository, `cargo run --release -- serve` does the
same as `treason serve`. Ctrl-C or SIGTERM stops it after ending every
note, and the project is lost unless it was saved. Several agents can
connect at once, and the music keeps playing when they disconnect. To
connect Claude Code:
`claude mcp add --transport http treason http://127.0.0.1:6434/mcp`.

Saved projects are TOML files in the projects directory, `<name>.toml`:
`format = 1`, the tempo, the time signature, and each track with its port
by name and its events one to a line, with times in ticks, 960 to a beat.

## Tips

- Ask the user which gear is on which port and channel, and use its device
  profile if there is one: names beat numbers.
- Edits land on the next bar line, so a change to a slow loop takes a
  moment to hear.
- Save after each idea the user likes. `undo` takes back what didn't work.
- Name tracks for what they play, so the user and other agents can follow.

## What treason doesn't do yet

- Make sound: it only sends MIDI.
- Record or follow MIDI input, or an external clock.
- Send program changes, pitch bend, aftertouch, NRPN or SysEx from the
  tools.
- Probability, ratchets or conditional steps.
- Arrange a song: every track loops. Change sections by editing tracks
  while playing, or by muting and unmuting them.
