# The engine

`cxtune::engine` keeps the dongle at the saved settings for a user interface to drive. The
tray application uses it; `cxtune watch` is the same engine without an interface.

## Threads and ownership

- `Engine::start(sink)` takes the machine-wide device lock (`NamedLock::device`, the named mutex
  `Global\cxtune-device`), reads `%APPDATA%\cxtune\config.toml` and starts one worker thread
  (`cxtune-engine`). It fails with `Error::Busy` if another cxtune process holds the lock and with
  `Error::Config` if the settings file is invalid; nothing has touched the dongle by then. The
  first `Event::State` follows the first enumeration, so it already says whether the dongle is
  connected.
- The worker thread is the only code that opens the HID device or the silent playback stream.
  The lock lives as long as the worker. Stopping the engine (`Engine::stop`, or dropping it)
  lets the worker finish what it is doing, save unsaved settings and release the lock.
- The lock is the existence of a named kernel mutex, not ownership of it: the first process to
  create the name holds it, Windows frees it when the process exits or crashes, and it can be
  dropped on any thread. `NamedLock::acquire(name)` takes any name, so a single-instance check
  for the user interface is `Local\...`.
- `Engine::handle()` gives a `Handle`: `send(Command)` never blocks, can be cloned and used
  from any thread, and is dropped silently once the engine has stopped.
- The sink passed to `start` runs on the worker thread and must return quickly. A user
  interface forwards each `Event` to its own thread.

## Commands, state and events

| `Command` | Effect |
|---|---|
| `SetMode(Mode)` | saved mode |
| `SetMinHissMs(u16)` | min-hiss settle time (20-2000 ms in steps of 10), used in min-hiss mode |
| `SetMicBoost(u8)` | analog mic boost 0-7 |
| `SetNoiseReduction(bool)`, `SetAgc(bool)` | microphone DSP bits |
| `SetSidetone(bool)`, `SetSidetoneLevel(u16)` | sidetone on/off and gain register 1-4096 |
| `Reapply` | applies the saved settings now, forced, and starts over after earlier failures |
| `Refresh` | reads the dongle now and publishes what it holds; writes nothing |

A setting command changes the saved settings and applies them to the dongle, now or when it
connects. A value out of range is rejected with an `ErrorKind::Invalid` error and changes
nothing.

An `Event` is one of:

- `State(Snapshot)`: sent once per change, never twice with the same content. A `Snapshot` holds
  the `Phase` (`Disconnected`, `Waiting`, `Applying`, `Connected`), the saved `settings`
  (`Config`), `device` (the `Readback` of every managed setting from the last read, `None`
  while disconnected or not yet read) and `error` (the open problem, if any). `in_sync()` is
  true when the device was read and holds every saved setting.
- `Warning(String)`: something to be aware of that stops nothing, such as a stream event near a
  DSP reload request.
- `Error(Failure)`: a problem, sent when it first appears or changes; a problem that persists
  through retries is not sent again. `Failure` has an `ErrorKind` and a message for the user.

| `ErrorKind` | Meaning | Retried |
|---|---|---|
| `Communication` | HID I/O failed: cable, USB reset, another program on the interface | 2, 4, 8, 16, then every 30 s |
| `Audio` | the silent stream could not be used | 5, 10, 20, 40, then every 60 s |
| `Refused` | a safety check refused (unrecognised patch RAM, unsupported firmware) | after 5, 15 and 60 s, then not |
| `Verify` | a write did not stick, or the device does not hold the settings after applying | after 10 and 60 s, then not |
| `Config` | the settings file could not be written | no; the next change writes it again |
| `Invalid` | a command carried a value out of range | no |

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
| `VERIFY` | 15 s | a connected dongle is read and compared with the saved settings |
| resume gap | 5 s | a gap this long in the worker loop's wall-clock rhythm counts as sleep |

- **Arrival and flapping.** A dongle seen after an absence is `Waiting` for `SETTLE`; if it is
  gone at any poll before then, the wait starts over on the next arrival and nothing was
  touched. Once the wait is over the dongle is read; a restarted dongle shows the stock patch
  and is brought to the saved settings, a dongle that is in order is left as it is.
- **Drift.** Every `VERIFY` the dongle is read (`mode::read_state`, about a dozen requests) and
  compared with the saved settings (`Readback::differences`). Only a difference leads to
  `settings::ensure`, which writes through the guarded patches only what differs. After
  `ensure` the dongle is read again; a difference that remains is a `Verify` failure.
- **Sleep.** The gap is measured in wall-clock time from the end of the previous pass of the
  loop, which the monotonic clock may not reflect across sleep. A clock set forward looks the
  same and only costs a verification.
- **Coalescing.** A command changes the saved settings in memory at once. The settings file and
  the dongle follow `COALESCE` after the first command of a burst, with whatever the values are
  by then; commands that arrive while the worker is busy are all taken before the next pass.
  Commands that cancel out write nothing, because the pass compares the dongle with the final
  settings.
- **Reports.** `Event::Warning` carries each stream event of a pass. A refused DSP setting
  keeps the report of the settings applied before it (`dsp::Attempt`), and the mode and mic
  boost are still applied.
- **Settings file.** Written by renaming a finished temporary file over `config.toml`, once per
  burst and once more when the engine stops if anything is unsaved. A failed write is reported
  as `Config`; the dongle still gets the settings.
- **Log.** `%LOCALAPPDATA%\cxtune\cxtune.log`, rotated at 256 KiB to `cxtune.log.1`.

## Tests

The supervisor runs against the fake dongle (`src/fake.rs`) with a clock the tests advance by
hand (`engine::testing::Rig`), so arrival, flapping, drift, coalescing, retries, sleep and
partial reports are checked without real time. A few tests run the whole engine on its worker
thread with real time (`engine::tests`).
