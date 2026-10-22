# The engine

`cxtune::engine` keeps the dongle at the settings the user chose, for a user interface to drive.
`cxtune watch` is the same engine without an interface.

## Threads and ownership

- `Engine::start(sink)` takes the machine-wide device lock (`NamedLock::device`, the named mutex
  `Global\cxtune-device`), reads `%APPDATA%\cxtune\config.toml` and starts one worker thread
  (`cxtune-engine`) and a watchdog thread (`cxtune-watchdog`). It fails with `Error::Busy` if
  another cxtune process holds the lock and with `Error::Config` if the settings file is invalid;
  nothing has touched the dongle by then. The first `Event::State` follows the first enumeration,
  so it already says whether the dongle is connected.
- `Engine::start_with(config_path, log_path, make_backend, sink)` is the same on any `Backend`
  with the settings file and the log where the caller says. It takes the device lock too: there is
  no way to start an engine on the real dongle without it.
- The worker thread is the only code that opens the HID device or the silent playback stream.
  The lock lives as long as the worker. Stopping the engine (`Engine::stop`, or dropping it)
  lets the worker finish what it is doing, save unsaved settings and release the lock.
  `Engine::is_running()` is false once the worker has ended, whether it was stopped or failed.
- The lock is the existence of a named kernel mutex, not ownership of it: the first process to
  create the name holds it, Windows frees it when the process exits or crashes, and it can be
  dropped on any thread. `NamedLock::acquire(name)` takes any name, so a single-instance check
  for the user interface is `Local\...`. A process whose engine is stuck on a request that never
  returns (see below) keeps the lock until it ends.
- `Engine::handle()` gives a `Handle`: `send(Command)` never blocks, can be cloned and used
  from any thread, and is dropped silently once the engine has stopped.
- The sink passed to `start` runs on the worker thread (and, for the watchdog's report, on the
  watchdog thread) and must return quickly. A user interface forwards each `Event` to its own
  thread. `Engine::stop` waits for the end of a mode change, which takes seconds, so a user
  interface calls it off its own thread.

## Commands, state and events

| `Command` | Effect |
|---|---|
| `SetMode(Mode)` | mode: default, low click, balanced or lowest noise |
| `SetMinHissMs(u16)` | lowest-noise soft-start time (20-2000 ms in steps of 10), used in lowest-noise mode |
| `SetBalancedDecayMs(u16)` | balanced stop decay time (100-2000 ms in steps of 10), used in balanced mode |
| `SetMicBoost(u8)` | analog mic boost 0-7 |
| `SetNoiseReduction(bool)`, `SetAgc(bool)` | microphone DSP bits |
| `SetSidetone(bool)`, `SetSidetoneLevel(u16)` | sidetone on/off and the gain register 1-4096, in UAC 1/256 dB units per the ROM; not exposed to the host; DSP loudness not measured (947 = +3.70 dB, 4096 = +16 dB; `patches::sidetone_register(db)` and `sidetone_db(register)` convert). Every level amplifies: only sidetone off is silent. |
| `SetSurround(bool)`, `SetSurroundLevel(u8)` | virtual surround on/off and its level 1-10 (experimental; the sound was not listened to) |
| `Test(TestRequest)` | a request of the Test tab; not a setting, nothing is saved (see The Test tab) |
| `Reapply` | applies the settings now, forced, and starts over after earlier failures and after a setting was left alone (see Drift) |
| `Refresh` | reads the dongle now and publishes what it holds; writes nothing |

A setting command changes the settings and applies them to the dongle, now or when it connects,
and writes them to the settings file. A value out of range is rejected with an
`ErrorKind::Invalid` error and changes nothing.

An `Event` is one of:

- `State(Snapshot)`: sent once per change, never twice with the same content. A `Snapshot` holds
  - `phase`: `Disconnected`, `Waiting` (the dongle is left alone while its firmware loads),
    `Applying`, `Connected`, or `Stopped` (the engine ended after an internal error);
  - `settings`: the settings the user chose, which the engine keeps the dongle at;
  - `settings_saved`: true when the settings file holds `settings`. False between a change and
    the write, and for as long as the write keeps failing;
  - `save_error`: why the settings file could not be written, until a write succeeds;
  - `device`: the `Readback` of every managed setting from the last read (`None` while
    disconnected or not yet read);
  - `error`: the open problem with the dongle, if any;
  - `test`: what the Test tab has running (see The Test tab).

  `in_sync()` is true when the device was read and holds every chosen setting.
- `Test(TestEvent)`: a message for the Test tab (see The Test tab).
- `Warning(String)`: something to be aware of that stops nothing: a stream event near a DSP
  reload request, or a setting that changed on the dongle without cxtune and was corrected.
- `Error(Failure)`: a problem, sent when it first appears or changes; a problem that persists
  through retries is not sent again. `Failure` has an `ErrorKind` and a message for the user.

| `ErrorKind` | Meaning | Retried |
|---|---|---|
| `Communication` | HID I/O failed: cable, USB reset, another program on the interface | 2, 4, 8, 16, then every 30 s |
| `Audio` | the silent stream could not be used | 5, 10, 20, 40, then every 60 s |
| `Refused` | a safety check refused (unrecognised patch RAM, unsupported firmware) | after 5, 15 and 60 s, then not |
| `Verify` | a write did not stick, or the device does not hold the settings after applying | after 10 and 60 s, then not |
| `Config` | the settings file could not be written (`save_error`) | 2, 4, 8, 16, then every 30 s, until it works |
| `Invalid` | a command carried a value out of range | no |
| `Drift` | something keeps changing a setting on the dongle; the engine leaves that setting alone (`error`) | no; see Drift |
| `Unresponsive` | a request to the dongle does not return (`error`) | clears when the request returns |
| `Fatal` | the engine stopped after an internal error (`error`, with `Phase::Stopped`) | no |

After the last retry the engine waits for a command, a `Reapply`, a `Refresh` (which reads but
does not start over) or the dongle coming back. A `Communication` failure while the dongle is no
longer enumerated is a disconnect and is not reported.

## What it does and when

All decisions are in `engine::supervisor`; the worker loop calls `command` with what arrived and
`step` when `next_wake` has passed.

| Constant | Value | Meaning |
|---|---|---|
| `POLL` | 1 s | presence check (HID enumeration, no device I/O) |
| `SETTLE` | 2 s | a dongle that appeared (or a computer that woke) is left alone this long |
| `COALESCE` | 150 ms | a command reaches the dongle and the settings file this long after the first of a burst |
| `VERIFY` | 15 s | a connected dongle is read and compared with the settings |
| `MAX_CORRECTIONS` | 3 | corrections of one setting that changed without cxtune, within `DRIFT_WINDOW` |
| `DRIFT_WINDOW` | 10 min | |
| resume gap | 5 s | a gap this long in the worker loop's wall-clock rhythm counts as sleep |
| `STALL` | 10 s | a request to the dongle that has not returned by then is reported |
| `STOP_WAIT` | 20 s | the longest `Engine::stop` waits for the worker |

- **Arrival and flapping.** A dongle seen after an absence is `Waiting` for `SETTLE`; if it is
  gone at any poll before then, the wait starts over on the next arrival and nothing was
  touched. Once the wait is over the dongle is read; a restarted dongle shows the stock patch
  and is brought to the settings, a dongle that is in order is left as it is.
- **Verification.** Every `VERIFY` the dongle is read (`mode::read_state`, about a dozen
  requests) and compared with the settings (`Readback::differences`). Only a difference leads to
  `settings::ensure`, which writes through the guarded patches only what differs. After
  `ensure` the dongle is read again; a difference that remains is a `Verify` failure.
- **Restart between two polls.** A dongle that held the settings at the last verification and
  now holds the vendor's (mode default, `$1017 = 0x3C` (boost 4), noise reduction on, AGC and
  sidetone off, sidetone level 947; `docs/firmware-notes.md`) while the settings differ from that
  has restarted without the poll seeing it go. It is handled like an arrival: what was learned
  about the dongle (including the corrections below) is forgotten, it is `Waiting` for `SETTLE`,
  and then it is brought to the settings. Nothing counts as drift. A dongle that keeps
  restarting points to its cable, its port or its power, so from the `WARN_RESTARTS`-th restart
  within `DRIFT_WINDOW` each restart also sends an `Event::Warning` with the count; the settings
  are applied again every time, and the warning does not stop anything. That makes the
  verification, not the poll, the one that finds a quick reset. A dongle that holds the vendor's
  values by the user's choice (nothing differs) is left alone.
- **Drift.** Any other difference found while the settings are the ones the dongle held at the
  last verification was not the user's doing: most likely another program. Each setting that
  differs is corrected and an `Event::Warning` says so. Every correction
  of the codec's DSP settings reloads the DSP, so a setting that something keeps changing is not
  fought for ever: after `MAX_CORRECTIONS` corrections of one setting within `DRIFT_WINDOW`, the
  next difference is not corrected. The snapshot then holds an `ErrorKind::Drift` error (and
  `in_sync()` is false), one `Event::Error` is sent, and the other settings are still corrected.
  A change of the settings, a `Reapply`, a replug, a restart or the dongle holding the setting
  again starts over.
- **Sleep.** The gap is measured in wall-clock time from the end of the previous pass of the
  loop, which the monotonic clock may not reflect across sleep. A clock set forward looks the
  same and only costs a verification.
- **Coalescing.** A command changes the settings in memory at once. The settings file and the
  dongle follow `COALESCE` after the first command of a burst, with whatever the values are by
  then; commands that arrive while the worker is busy are all taken before the next pass.
  Commands that cancel out write nothing, because the pass compares the dongle with the final
  settings.
- **Reports.** `Event::Warning` carries each stream event of a pass. A refused DSP setting
  keeps the report of the settings applied before it (`dsp::Attempt`), and the mode and mic
  boost are still applied.
- **Settings file.** Written by renaming a finished temporary file over `config.toml`, once per
  burst. A failed write (the file held open by another program, a full disk) is reported once as
  `Config` and stays in `save_error`; the dongle still gets the settings. The write is tried
  again after 2, 4, 8, 16 and then every 30 s, sooner after the next change, and once more when
  the engine stops. `settings_saved` is false until a write succeeds.
- **Log.** `%LOCALAPPDATA%\cxtune\cxtune.log`, rotated at 256 KiB to `cxtune.log.1`. A program
  that holds either file open without delete sharing blocks the rename; the current file is then
  emptied instead, so the two files stay within 512 KiB either way.

## The Test tab

`Command::Test(TestRequest)` drives the measurements and output tests of the user interface.
Nothing in it is a setting. The microphone is opened on demand (by VID/PID, shared mode) and
closed by the engine as soon as nothing needs it. A request that cannot be run is answered with
`TestEvent::Failed` and changes nothing; `Snapshot::error` is not touched.

| `TestRequest` | What it does |
|---|---|
| `Meter(bool)` | opens or closes the microphone; while it is open `TestEvent::Level` reports the RMS and peak level of every 50 ms (about 20 per second). Opening it is refused (`Invalid`) while a click test, noise listening or playback runs, because those need the output at rest |
| `NoiseFloor { seconds }` | the median of the 50 ms levels over 1-60 s, after the warm-up |
| `Record { seconds }` | records 1-30 s into memory, after the warm-up |
| `PlayRecording` | plays the recording through the dongle, scaled down as a whole if its peak is above -12 dBFS |
| `NoiseReductionAb { seconds }` | measures 1-30 s with noise reduction on and as long with it off, and puts the setting back (also when the test fails or is cancelled) |
| `ClickTest` | waits until the output is idle, rests, then plays a 1 kHz tone at -40 dBFS, 300 ms with 10 ms fades |
| `NoiseListen` | closes the microphone and waits until the output is idle; no stream is open afterwards |
| `Cancel` | ends the running test, puts back what it changed and closes the microphone unless the meter is on |

- **State.** `Snapshot::test` is a `TestState`: `meter` (the level meter is on), `running` (the
  `TestKind` that runs, at most one) and `has_recording`. `TestEvent::Progress` names the step
  (`WarmingUp`, `Measuring`, `Recording`, `MeasuringWithNoiseReduction`,
  `SwitchingNoiseReduction`, `MeasuringWithoutNoiseReduction`, `WaitingForIdle`, `Resting`,
  `Playing`) and the milliseconds left in it, every 100 ms, for a countdown. `TestEvent::Done`
  carries the outcome (`NoiseFloor`, `Recorded`, `Played`, `NoiseReductionAb`, `ClickTest`,
  `NoiseListen`, `Cancelled`).
- **The dongle while a test runs.** A running test holds the dongle: the saved settings wait until
  it ends. The meter alone does not, so a setting can be changed and watched on the meter. A
  disconnect, a sleep or `Engine::stop` ends the test and closes everything it opened. A
  noise reduction comparison that `Engine::stop` ends puts the setting back at once; one that a
  disconnect ends tries to, and a warning is logged when the write fails. One that a sleep ends
  leaves the dongle alone: the put-back is pending until the settling wait of 2 s after the wake
  is over, and the first settings pass after it writes it. A pending put-back stays through a
  further sleep and through a test that does not switch noise reduction; a comparison that starts
  meanwhile takes the pending value as its own original and puts it back itself when it ends. If
  the dongle disconnects or the engine stops before that pass, the pending put-back is dropped.
  A test request is not held back during that settling wait, so a comparison started in it writes
  `$1151` before the wait is over (the write is guarded like every other).
- **Warm-up.** The first 4 s after the microphone opens are not measured or recorded: the codec
  delivers digital silence and a click first, and in balanced mode the meter showed higher
  levels for a while longer (`docs/firmware-notes.md`; the 4 s is provisional until it is
  checked in a quiet room). A test started while the meter is on does not wait again, except that
  a noise reduction comparison lets the level settle for 1 s after its first switch, as it does
  after its second. A microphone that delivers nothing for 3 s ends the test with an `Audio`
  error.
- **The output after a capture.** In balanced mode a capture opens the driver, and the output is
  idle again only after the stop decay time. After every microphone close the engine reads the
  output registers every 100 ms until `mode::Output::is_idle` holds for the mode (15 s at most,
  then a warning) and logs the time. The other modes change nothing for a capture and are not
  watched. `ClickTest` and `NoiseListen` wait for the same state before they go on, and the
  click test rests 500 ms before it plays.
- **The tone and the recording.** The clip starts with silence of the mode's mute delay
  (`mode::mute_delay_ms`: none in default and low click, 650 ms in balanced, the soft-start
  time plus 150 ms in lowest noise) plus 200 ms, so the first samples are not swallowed by the
  power-up, and ends with 300 ms of silence. It is played in an exclusive 16-bit stereo stream
  when possible and a shared one otherwise, and the stream is closed when the clip is over.
- **The noise reduction comparison.** It switches `$1151` bit 2 and requests a reload (NEWC)
  like the settings do, leaves 1 s after the switch for the level to settle and checks that the
  codec applied each step (`$00A4` bit 7).

## A request that never returns

hidapi waits for the answer to a HID request without a time limit, and it gives no handle to
cancel the wait. A dongle or driver that stops answering therefore blocks the worker thread inside
the call. Every call into the device (each HID request, enumeration, opening the dongle and the
silent stream) is marked while it runs, and the watchdog thread checks the mark four times per
`STALL`:

- A call that has run for `STALL` is reported once: an `Event::Error` of kind `Unresponsive` and
  a repeat of the latest state with that error, asking the user to replug the dongle. The error
  is not retried; it is taken back, with a fresh state, when the call returns. Replugging the
  dongle usually makes the operating system end the request with an error, after which the engine
  goes on as after any disconnect.
- `Engine::stop` does not wait for a stuck call. It waits for the worker for at most `STOP_WAIT`
  otherwise, and leaves the worker behind when it does not finish. The worker keeps the device
  lock until its call returns or the process ends. A new `Engine::start` in the same process is
  refused with `Error::Busy` and a message that names that engine ("the previous engine of this
  program is still waiting for the dongle to answer"), not another cxtune process.
  `worker_left_behind()` tells a user interface which of the two it is.
- When the call returns, the worker that was left behind finishes its pass and ends. It may call
  the sink once or more after `stop` has returned, so a sink must cope with events after `stop`;
  a user interface that has dropped its receiver ignores them.
- The watchdog checks the mark again after it has the sink to itself, so that a report is not
  published after the call has returned and the worker has published the state without the error.

The marks cover the dongle, not everything the worker does: dropping the silent stream (it joins
the audio thread), the sink and the write of the settings file are not marked. A hang there is
not reported, but `Engine::stop` is still bounded by `STOP_WAIT`.

Cancelling the request instead (own Win32 code with overlapped I/O) would replace the transport
that was verified on the real dongle for a situation that can only be reproduced with a faulty
cable; reporting the stall and keeping the interface responsive needs no change to it.

## Failure of the engine

A panic on the worker thread (including one in the sink) is caught. The worker saves unsaved
settings, publishes a last `Event::Error` of kind `Fatal` and a state in `Phase::Stopped` with the
same error, and ends; `Engine::is_running()` is then false and the lock is released. Nothing is
applied after that: the user interface shows the error and offers a restart.

## Tests

The supervisor runs against the fake dongle (`src/fake.rs`) with a clock the tests advance by
hand (`engine::testing::Rig`), so arrival, flapping, drift, coalescing, retries, sleep, partial
reports, the settings file's retries and the whole Test tab (a scripted microphone and clip
player stand in for the audio streams) are checked without real time. A few tests run the whole
engine on its worker thread with real time (`engine::tests`): commands from other threads,
sleep, the lock, a panic, a stuck call and its recovery. Tests keep their files in directories
that are removed when the test ends, also when it fails (`testdir`).
