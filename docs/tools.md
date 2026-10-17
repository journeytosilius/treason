# treason's MCP tools

<!-- Made by `cargo test -p treason-mcp` from the server's tool list. Don't edit it by hand. -->

Every tool that `treason serve` offers over MCP: what it does, what it
takes and what it returns. This file is made from the server's own tool
list, so it matches the code. After changing a tool, update it with
`TREASON_UPDATE_DOCS=1 cargo test -p treason-mcp`. [guide.md](guide.md)
explains how the tools fit together, with examples.

| Tool | What it does |
|---|---|
| [`add_track`](#add_track) | Adds a track that loops one empty bar, with a program to set the sound of its channel, and a bend range for its pitch bend, if you like. |
| [`edit_track`](#edit_track) | Renames a track, moves it to another port, channel or device, or changes its program or bend range, as one edit. |
| [`export_midi`](#export_midi) | Exports the project as a Standard MIDI File that a DAW can open: a track for each unmuted track, named after it, with the tempo and time signature. |
| [`get_guide`](#get_guide) | The guide to driving treason: how projects, tracks and loops work, what each tool is for, the names and notations, the limits, what the errors mean and how to connect gear, with examples of every call. Read it once before you start. |
| [`get_project`](#get_project) | Shows the project: tempo, time signature and every track, with each clip as step strings or notes, in the form set_steps or set_notes takes, and its controls in the form set_controls takes. |
| [`list_devices`](#list_devices) | Lists the device profiles. |
| [`list_ports`](#list_ports) | Lists the MIDI output ports that tracks can play on. |
| [`list_projects`](#list_projects) | Lists the saved projects, newest first, for load_project. |
| [`load_project`](#load_project) | Loads a project saved with save_project. |
| [`panic`](#panic) | Ends every sounding note on every port at once, and puts the pedals, pitch bend and pressure back at rest on every channel, for stuck notes and bends. |
| [`play`](#play) | Starts playing from bar 1, or from `from_bar`. |
| [`redo`](#redo) | Redoes the last undone edit. |
| [`remove_track`](#remove_track) | Removes a track. |
| [`save_project`](#save_project) | Saves the project under a name, so that load_project can bring it back, even after treason restarts. |
| [`set_controls`](#set_controls) | Sets how a track moves its synth's controls, such as the mod wheel or a filter's cutoff, by sending MIDI control changes (CC) through the loop, and how it bends pitch (pitch_bend) and presses on the keys (pressure, channel aftertouch). |
| [`set_muted`](#set_muted) | Mutes or unmutes a track, at the next bar while playing. |
| [`set_notes`](#set_notes) | Replaces a track's notes, for basslines, chords and melodies. |
| [`set_steps`](#set_steps) | Replaces a track's notes with a step pattern: one step string per voice, the usual way to write drums. |
| [`set_tempo`](#set_tempo) | Sets the tempo. |
| [`set_time_signature`](#set_time_signature) | Sets the time signature, and with it the bar length. |
| [`status`](#status) | Shows whether the sequencer is playing, where, and how fast, and the project's name and whether it has unsaved changes. |
| [`stop`](#stop) | Stops playing and ends every sounding note, releasing pedals, recentring pitch bend and lifting pressure. |
| [`undo`](#undo) | Undoes the last edit. |

## add_track

Adds a track that loops one empty bar, with a program to set the sound of its channel, and a bend range for its pitch bend, if you like. Returns the track: pass its id to the other tools.

Input:

| Field | Type | Description |
|---|---|---|
| `name` | string | A name for the track, such as "drums" or "bass". |
| `bank_lsb` | integer, optional | Bank select LSB (controller 32), 0-127, sent before a program given by number, on gear with banks. |
| `bank_msb` | integer, optional | Bank select MSB (controller 0), 0-127, sent before a program given by number, on gear with banks. |
| `bend_range` | integer, optional | How many semitones pitch_bend reaches either way, 0-127, so 8191 bends up this far and -8192 down: sent to the track's channel as RPN 0, pitch bend sensitivity, along with its program. Tracks on the same port and channel share one. Without it, the device keeps its own, often 2. Most synths take up to 24. |
| `channel` | integer, optional | The MIDI channel, 1-16. Defaults to the device's channel, or 1. |
| `device` | string, optional | A device profile id, from list_devices. It lets set_steps and set_notes use the device's voice names. |
| `port` | string, optional | The output port, from list_ports. Defaults to the first port. |
| `program` | [`ProgramRef`](#programref), optional | The sound to select on the track's channel: a program number, 0-127, or one of the device's programs from list_devices, such as "electric_bass_finger" on device "gm". Sent when playback starts, after a jump, and at the bar where it changes. Tracks on the same port and channel share one program. Without it, the device keeps the sound it has. |

Returns [`TrackView`](#trackview).

## edit_track

Renames a track, moves it to another port, channel or device, or changes its program or bend range, as one edit. Its clip stays as it is. While playing, the change takes effect at the next bar. Returns the track.

Input:

| Field | Type | Description |
|---|---|---|
| `track` | integer | The track's id. |
| `bank_lsb` | integer, optional | Bank select LSB (controller 32), 0-127, for the program. |
| `bank_msb` | integer, optional | Bank select MSB (controller 0), 0-127, for the program. |
| `bend_range` | integer, optional | Another bend range, as in add_track, or null to stop sending one. |
| `channel` | integer, optional | Another MIDI channel, 1-16. Without it the channel stays, even when the device changes. |
| `device` | string, optional | Another device profile id, from list_devices. Later set_steps calls use its voice names; the notes already in the clip stay as they are. |
| `name` | string, optional | A new name. |
| `port` | string, optional | Another output port, from list_ports. |
| `program` | [`ProgramRef`](#programref), optional | Another program, as in add_track, or null to stop sending one. Its bank comes with it: give bank_msb and bank_lsb again if it needs them. |

Returns [`TrackView`](#trackview).

## export_midi

Exports the project as a Standard MIDI File that a DAW can open: a track for each unmuted track, named after it, with the tempo and time signature. Muted tracks are left out, as they are from playback. The file plays from bar 1 for `bars` bars, by default until every track's loop ends on the same bar line. It goes in the exports directory as <name>.mid, by default under the project's own name; replacing a different MIDI file needs overwrite: true. Returns where the file is.

Input:

| Field | Type | Description |
|---|---|---|
| `bars` | integer, optional | How many bars to export, from bar 1: 1 to 1000. Defaults to enough for every track's loop to end on the same bar line, so that each pattern is complete. |
| `name` | string, optional | The MIDI file's name, without ".mid": up to 64 letters, digits, spaces, dashes and underscores, beginning with a letter or digit. Defaults to the project's own name, which status shows. |
| `overwrite` | boolean, optional | Replace the MIDI file of that name. Not needed for the project's own name. |

Returns text.

## get_guide

The guide to driving treason: how projects, tracks and loops work, what each tool is for, the names and notations, the limits, what the errors mean and how to connect gear, with examples of every call. Read it once before you start.

Read only: it changes nothing.

No input: call it with `{}`.

Returns text.

## get_project

Shows the project: tempo, time signature and every track, with each clip as step strings or notes, in the form set_steps or set_notes takes, and its controls in the form set_controls takes.

Read only: it changes nothing.

No input: call it with `{}`.

Returns:

| Field | Type | Description |
|---|---|---|
| `tempo` | number | Beats per minute. |
| `time_signature` | string | Such as "4/4". |
| `tracks` | list of [`TrackView`](#trackview) | Every track. Each loops its own clip. |

## list_devices

Lists the device profiles. A track with a device names its notes by voice, such as "kick" or "closed_hat", its controls by parameter, such as "cutoff", and its sound by program, such as "electric_bass_finger" on "gm", the General MIDI sounds.

Read only: it changes nothing.

No input: call it with `{}`.

Returns:

| Field | Type | Description |
|---|---|---|
| `devices` | list of [`DeviceView`](#deviceview) | Every device profile. |

## list_ports

Lists the MIDI output ports that tracks can play on.

Read only: it changes nothing.

No input: call it with `{}`.

Returns:

| Field | Type | Description |
|---|---|---|
| `ports` | list of string | Port names, for add_track. The first is the default. |

## list_projects

Lists the saved projects, newest first, for load_project.

Read only: it changes nothing.

No input: call it with `{}`.

Returns:

| Field | Type | Description |
|---|---|---|
| `more` | integer | How many older projects there are beyond the list. |
| `projects` | list of string | Project names for load_project, newest first. |

## load_project

Loads a project saved with save_project. It replaces the current project and its undo history, so it refuses while the current project has unsaved changes, unless discard_changes is true. While playing, the loaded project takes over like an edit: its tempo at once, its tracks at the next bar. Returns the project, as get_project shows it.

Input:

| Field | Type | Description |
|---|---|---|
| `name` | string | The saved project's name, from list_projects. |
| `discard_changes` | boolean, optional | Load even if the current project has unsaved changes, which are then lost. |

Returns the same as [`get_project`](#get_project).

## panic

Ends every sounding note on every port at once, and puts the pedals, pitch bend and pressure back at rest on every channel, for stuck notes and bends. Playback carries on.

No input: call it with `{}`.

Returns text.

## play

Starts playing from bar 1, or from `from_bar`. Every track loops its clip until stop. Gear on ports that get MIDI clock starts with it, from the same bar.

Input:

| Field | Type | Description |
|---|---|---|
| `from_bar` | integer, optional | The bar to start from, counting from 1. Defaults to 1. |

Returns text.

## redo

Redoes the last undone edit.

No input: call it with `{}`.

Returns text.

## remove_track

Removes a track. While playing, it stops at the next bar. undo brings it back.

Input:

| Field | Type | Description |
|---|---|---|
| `track` | integer | The track's id. |

Returns text.

## save_project

Saves the project under a name, so that load_project can bring it back, even after treason restarts. It keeps everything: the tempo, the time signature, and every track with its clip, port, channel, device and mute. Without a name, saves under the project's own name, the one it was last saved as or loaded from. Saving over a different saved project needs overwrite: true.

Input:

| Field | Type | Description |
|---|---|---|
| `name` | string, optional | The name to save under, such as "acid jam" or "dub-techno_2": up to 64 letters, digits, spaces, dashes and underscores, beginning with a letter or digit. Defaults to the project's own name, which status shows. |
| `overwrite` | boolean, optional | Replace the saved project of that name. Not needed to save again under the project's own name. |

Returns text.

## set_controls

Sets how a track moves its synth's controls, such as the mod wheel or a filter's cutoff, by sending MIDI control changes (CC) through the loop, and how it bends pitch (pitch_bend) and presses on the keys (pressure, channel aftertouch). A track can send controls alone, with no notes: modulation for a synth that plays its own notes, for example. Each control is a list of points, values at times in beats, that it jumps to or ramps to from the point before. A two-bar mod-wheel swell: {"track": 3, "controls": {"modulation": {"points": [{"at": 0, "value": 0}, {"at": 8, "value": 127, "ramp": "linear"}]}}}. A sine-wave LFO on the cutoff, once a beat: {"track": 3, "controls": {"cutoff": {"points": [{"at": 0, "value": 32}, {"at": 0.5, "value": 96, "ramp": "smooth"}, {"at": 1, "value": 32, "ramp": "smooth"}], "repeat_beats": 1}}}. A bend up to the top of the bend range over beat 4, back at the centre for the next bar: {"track": 3, "controls": {"pitch_bend": {"points": [{"at": 0, "value": 0}, {"at": 3, "value": 0}, {"at": 4, "value": 8191, "ramp": "smooth"}]}}}. The track's notes stay, and so do the controls the call leaves out; a control with no points is removed. While playing, the change takes effect at the next bar. Returns the track as it will play.

Input:

| Field | Type | Description |
|---|---|---|
| `controls` | map of names to [`ControlLane`](#controllane) | The controls to set, keyed by a parameter of the track's device (see list_devices), a standard name, or a controller number, 0-119; or pitch_bend, for pitch bend, or pressure, for channel pressure (aftertouch). The standard names, with their numbers: modulation (1, the mod wheel), breath (2), foot (4), portamento_time (5), volume (7), balance (8), pan (10), expression (11), sustain (64), portamento (65), sostenuto (66), soft_pedal (67), legato (68), resonance (71), release (72), attack (73), cutoff (74), decay (75), vibrato_rate (76), vibrato_depth (77), vibrato_delay (78), reverb (91), tremolo (92), chorus (93), detune (94) and phaser (95). |
| `track` | integer | The track's id. |
| `loop_beats` | number, optional | The loop length in beats, at most 1024, on a track without notes: on a track with notes, they set the loop. Defaults to the track's loop, or on a track with nothing else, to the end of the last point or repeat, rounded up to a whole bar. A shorter loop cuts off the track's other controls at its end. |

Returns [`TrackView`](#trackview).

## set_muted

Mutes or unmutes a track, at the next bar while playing. A muted track keeps its clip but plays nothing.

Input:

| Field | Type | Description |
|---|---|---|
| `muted` | boolean | True to silence the track, false to hear it again. |
| `track` | integer | The track's id. |

Returns text.

## set_notes

Replaces a track's notes, for basslines, chords and melodies. Name each note by pitch, such as "C4" (middle C), "F#2" or "Bb3", by MIDI note number, or by a voice of the track's device, and time it in beats from the start of the loop. For example, a bass figure: {"track": 2, "notes": [{"note": "C2", "start": 0, "length": 0.5}, {"note": "Eb2", "start": 1.5, "length": 0.5}]}. The track's controls stay where they are, cut off by a shorter loop; to fill a longer one, set them again with set_controls. While playing, the new clip starts at the next bar. Returns the track as it will play: as step strings if the notes fit a step grid, otherwise as notes.

Input:

| Field | Type | Description |
|---|---|---|
| `notes` | list of [`NoteSpec`](#notespec) | The notes, in any order, at most 1024. Notes that start together make a chord. Two notes of the same pitch can't overlap. |
| `track` | integer | The track's id. |
| `loop_beats` | number, optional | The loop length in beats, at most 1024. Defaults to the end of the last note, rounded up to a whole bar. A note can run past the end of the loop into the next pass. |

Returns [`TrackView`](#trackview).

## set_steps

Replaces a track's notes with a step pattern: one step string per voice, the usual way to write drums. The track's controls stay where they are, cut off by a shorter loop; to fill a longer one, set them again with set_controls. While playing, the new pattern starts at the next bar. Returns the track with its pattern as it will play.

Input:

| Field | Type | Description |
|---|---|---|
| `steps` | map of names to string | One step string per voice, keyed by the device's voice names (or their aliases) or by MIDI note number. Each character is a step: "x" a hit, "X" an accent, "." or "-" a rest. Spaces and "\|" are ignored. Example: {"kick": "x...x...x...x...", "snare": "....x.......x...", "closed_hat": "..x...x...x...x."}. The loop is as long as the longest string. A shorter one repeats to fill it, so {"closed_hat": "..x."} plays an off-beat hat in every beat; its length must divide the loop's. |
| `track` | integer | The track's id. |
| `steps_per_beat` | integer, optional | Steps per beat (a quarter note): 1, 2, 3, 4, 6, 8, 12, 16, 24 or 32. Defaults to 4, so each step is a sixteenth note; 3 gives eighth-note triplets. |

Returns [`TrackView`](#trackview).

## set_tempo

Sets the tempo. It changes at once, even while playing.

Input:

| Field | Type | Description |
|---|---|---|
| `bpm` | number | Beats per minute, 20 to 400. |

Returns text.

## set_time_signature

Sets the time signature, and with it the bar length. Changes to clips take effect on bar lines.

Input:

| Field | Type | Description |
|---|---|---|
| `denominator` | integer | The note value of a beat: 1, 2, 4, 8, 16 or 32. |
| `numerator` | integer | Beats per bar, 1 to 32. |

Returns text.

## status

Shows whether the sequencer is playing, where, and how fast, and the project's name and whether it has unsaved changes.

Read only: it changes nothing.

No input: call it with `{}`.

Returns:

| Field | Type | Description |
|---|---|---|
| `bar` | integer | The current bar, counting from 1. |
| `beat` | integer | The beat within the bar, counting from 1. |
| `playing` | boolean | Whether the sequencer is playing. |
| `tempo` | number | Beats per minute. |
| `unsaved_changes` | boolean | Whether the project has changed since it was last saved or loaded. |
| `project` | string, optional | The name the project was last saved as or loaded from, which save_project uses when given no name. Missing until then. |

## stop

Stops playing and ends every sounding note, releasing pedals, recentring pitch bend and lifting pressure. The position stays. Gear on ports that get MIDI clock stops too.

No input: call it with `{}`.

Returns text.

## undo

Undoes the last edit.

No input: call it with `{}`.

Returns text.

## Types

The types that the tools above take and return.

### ControlLane

How one control moves through the loop.

| Field | Type | Description |
|---|---|---|
| `points` | list of [`ControlPoint`](#controlpoint) | The control's values, in any order, at most 8192. An empty list removes the control. Successive jumps at the same time are sent in their list order, including repeated values. |
| `repeat_beats` | number, optional | Repeat the points every this many beats through the loop, such as 1 for an LFO that cycles once a beat. Defaults to the whole loop. A repeat that doesn't divide the loop starts again with it. |

### ControlPoint

A control's value at a point in the loop.

| Field | Type | Description |
|---|---|---|
| `at` | number | When, in beats from the start of the loop, or of each repeat: 0 is the first beat, 1.5 the offbeat after the second. Rounds to 1/960 of a beat. A ramp can end at the end of the loop or repeat, which is where the next pass starts again from the first point. |
| `value` | integer | The value, 0-127. For pitch_bend, -8192 to 8191: 0 is the centre, 8191 bends up by the track's bend_range and -8192 down by it, so with a range of 2, a whole tone up is 8191 and a semitone 4096. |
| `ramp` | [`Ramp`](#ramp), optional | How the value gets here from the point before: "linear" ramps evenly, and "smooth" eases out and in, so smooth ramps between a low and a high make a sine wave. Without it, the value jumps here. The first point can't ramp, since nothing comes before it. A ramp must end later than the point before; same-time points must jump. |

### ControlView

One control of a loop, as points in the form set_controls takes.

| Field | Type | Description |
|---|---|---|
| `points` | list of [`PointView`](#pointview) | The control's values, by time. Written back with set_controls and the track's loop_beats, they play the same, though they may not be the points first written: a repeat reads back as every pass of it. |

### DeviceView

What a synth or drum machine calls its notes and controls.

| Field | Type | Description |
|---|---|---|
| `aliases` | map of names to string | Other names for voices, such as "bd" for "kick". |
| `default_channel` | integer | The MIDI channel it listens on by default, 1-16. |
| `id` | string | Use this id in add_track. |
| `name` | string | The device's name. |
| `params` | map of names to integer | Parameter names and their MIDI CC numbers. |
| `programs` | map of names to [`ProgramView`](#programview) | Program names, for add_track and edit_track, and the program each selects. |
| `voices` | map of names to integer | Voice names and their MIDI notes. On a drum machine, one per sound. |
| `manufacturer` | string, optional | Who makes it. |

### NoteRef

A note: a MIDI note number, or a name.

One of:

- integer: A MIDI note number, 0-127.
- string: A voice of the track's device, such as "kick", or a pitch such as "C4" (middle C, note 60), "F#2" or "Bb3".

### NoteSpec

One note.

| Field | Type | Description |
|---|---|---|
| `length` | number | How long it lasts, in beats: 1 is a quarter note in 4/4, 0.5 an eighth. |
| `note` | [`NoteRef`](#noteref) | Which note. |
| `start` | number | When it starts, in beats from the start of the loop: 0 is the first beat, 1.5 the offbeat after the second. Rounds to 1/960 of a beat. |
| `velocity` | integer, optional | How hard, 1-127. Defaults to 100. |

### NoteView

One note of a loop.

| Field | Type | Description |
|---|---|---|
| `length` | number | How long it lasts, in beats. |
| `note` | string | The device's voice for the note if it has one, otherwise its pitch, such as "C4". A decimal MIDI number if the pitch name would resolve to a different device voice or alias. |
| `start` | number | When it starts, in beats from the start of the loop. |
| `velocity` | integer | How hard, 1-127. |

### PatternView

A loop written as step strings.

| Field | Type | Description |
|---|---|---|
| `steps` | map of names to string | One step string per voice, with a space between beats: "x" a hit, "X" an accent, "." a rest. |
| `steps_per_beat` | integer | Steps per beat, as in set_steps. |

### PointView

A control's value at a point in the loop.

| Field | Type | Description |
|---|---|---|
| `at` | number | When, in beats from the start of the loop. |
| `value` | integer | The value: 0-127, or for pitch_bend, -8192 to 8191 around the centre at 0. |
| `ramp` | [`Ramp`](#ramp), optional | How the value gets here from the point before. Missing where it jumps. |

### ProgramRef

A program: its number, or a name from the track's device.

One of:

- integer: A program number, 0-127, as sent. A manual that numbers programs from 1 means one more than this.
- string: One of the device's programs, from list_devices, such as "electric_bass_finger".

### ProgramView

A device's program.

| Field | Type | Description |
|---|---|---|
| `program` | integer | The program number, 0-127. |
| `bank_lsb` | integer, optional | Bank select LSB, sent next, for a program in a bank. |
| `bank_msb` | integer, optional | Bank select MSB, sent first, for a program in a bank. |

### Ramp

How a control moves from one point to the next.

One of:

- `"linear"`: At an even rate.
- `"smooth"`: Slowly at both ends and fastest midway, along half a sine wave.

### TrackView

A track.

| Field | Type | Description |
|---|---|---|
| `channel` | integer | The MIDI channel, 1-16. |
| `events` | integer | How many notes, control changes and other events the loop holds. |
| `id` | integer | Refers to the track in other tools. |
| `loop_beats` | number | The length of the track's loop, in beats. |
| `muted` | boolean | A muted track plays nothing. |
| `name` | string | The track's name. |
| `port` | string | The output port. |
| `bank_lsb` | integer, optional | Bank select LSB, sent before a program shown by number. |
| `bank_msb` | integer, optional | Bank select MSB, sent before a program shown by number. |
| `bend_range` | integer, optional | How many semitones pitch_bend reaches either way on its channel. Missing when the track sets none, leaving the device's own. |
| `controls` | map of names to [`ControlView`](#controlview), optional | The loop's controls (MIDI CC), pitch bend and pressure in the form set_controls takes, by name. Missing when it sends none. |
| `device` | string, optional | The device profile that names its voices and parameters, if any. |
| `notes` | list of [`NoteView`](#noteview), optional | The loop's notes in the form set_notes takes, by start. Shown for clips that aren't step patterns, such as melodies. |
| `pattern` | [`PatternView`](#patternview), optional | The loop's notes as step strings, in the form set_steps takes. Shown when every note sits on a step grid at set_steps' velocities; missing for other clips, such as melodies. |
| `program` | [`ProgramRef`](#programref), optional | The sound its channel is set to: the device's name for the program, or its number. Missing when the track sets none. |
