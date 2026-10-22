use std::path::Path;
use std::process::ExitCode;

use cxtune::config::{self, Config};
use cxtune::engine::{Engine, Event, Phase, Snapshot};
use cxtune::lock::NamedLock;
use cxtune::mode::{self, Mode, State};
use cxtune::patches::{
    APPLIED_AGC, APPLIED_NR, DSP_AGC, DSP_NR, DSP_SIDETONE, MIXER_SIDETONE, ROM_ID, SIDETONE_MAX,
    SIDETONE_MIN,
};
use cxtune::{Error, Result, audio, dsp, hex, hid, settings};

const USAGE: &str = "\
usage: cxtune <command>

  status                  read and show the device state (read-only)
  apply <mode>            apply an output mode and remember it:
                          stock | quiet | balanced | min-hiss
  tune <ms>               set the min-hiss settle time (20-2000 in steps of 10) and remember it
  mic-boost <0-7>         set analog mic boost ($1017 = 0x38 + n) and remember it
  mic-nr <on|off>         microphone noise reduction (device default on) and remember it
  mic-agc <on|off>        microphone AGC (experimental, device default off) and remember it
  sidetone <on|off>       microphone in the headphones (experimental, device default off)
                          and remember it
  sidetone-level <1-4096> sidetone gain register (experimental, unit not measured, device
                          default 947) and remember it; start low, sidetone can be loud
  dump <out-dir>          read the ROM and EEPROM (read-only) into <out-dir>/rom.bin and
                          eeprom.bin and compare their SHA-256 with the reference unit's
  watch                   keep the remembered settings applied across replugs; holds the
                          dongle for as long as it runs, so other commands are refused

Changes live in volatile RAM only; replugging the dongle restores the vendor patch.";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match args.as_slice() {
        ["status"] => locked(status),
        ["apply", name] => locked(|| apply(name)),
        ["tune", ms] => locked(|| tune(ms)),
        ["mic-boost", n] => locked(|| mic_boost(n)),
        ["mic-nr", v] => locked(|| {
            switch(
                "mic noise reduction",
                v,
                dsp::set_noise_reduction,
                |cfg, on| cfg.mic_nr = Some(on),
            )
        }),
        ["mic-agc", v] => {
            locked(|| switch("mic AGC", v, dsp::set_agc, |cfg, on| cfg.mic_agc = Some(on)))
        }
        ["sidetone", v] => locked(|| sidetone(v)),
        ["sidetone-level", v] => locked(|| sidetone_level(v)),
        ["dump", dir] => locked(|| dump(dir)),
        ["watch"] => watch(),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Runs `command` while this process holds the lock that keeps other cxtune processes away from
/// the dongle, or fails without running it when another process holds the lock.
fn locked(command: impl FnOnce() -> Result<()>) -> Result<()> {
    let _lock = NamedLock::device()?;
    command()
}

fn open() -> Result<hid::HidTransport> {
    hid::open(&hid::api()?)
}

fn status() -> Result<()> {
    let s = mode::read_state(&mut open()?)?;
    print_state(&s);
    Ok(())
}

fn dump(dir: &str) -> Result<()> {
    println!("reading the ROM and the EEPROM (read-only), this takes about 15 seconds");
    let dump = cxtune::dump::read(&mut open()?)?;
    dump.save(Path::new(dir))?;
    println!("wrote rom.bin and eeprom.bin to {dir}");
    print!("{}", dump.report());
    Ok(())
}

fn rom_note(rom: &[u8; ROM_ID.len()]) -> String {
    if *rom == ROM_ID {
        "matches the supported ROM".into()
    } else {
        format!(
            "DOES NOT MATCH the supported ROM ({}); cxtune writes nothing",
            hex(&ROM_ID)
        )
    }
}

fn mode_text(s: &State) -> &'static str {
    if s.rom != ROM_ID {
        "unknown (ROM identity does not match)"
    } else {
        s.mode().map_or("unrecognised or half-changed", Mode::name)
    }
}

fn print_state(s: &State) {
    println!(
        "$1001 ROM identity:   {}  {}",
        hex(&s.rom),
        rom_note(&s.rom)
    );
    let mode = mode_text(s);
    match s.settle_ms() {
        Some(ms) if s.mode() == Some(Mode::MinHiss) => {
            println!("mode:                 {mode} (settle {ms} ms)")
        }
        _ => println!("mode:                 {mode}"),
    }
    let hook = match s.hook_config() {
        Some((true, false)) => "JMP to the vendor routine (driver forced on)",
        Some((true, true)) => "JMP to the window routine",
        Some((false, false)) => "RTS, vendor operands (ROM idle power-down)",
        Some((false, true)) => "RTS, operands point at the window",
        None => "UNRECOGNISED",
    };
    println!("$14B1 hook30:         {}  {hook}", hex(&s.hook));
    for (i, (addr, hold)) in [0x16C9u16, 0x17CA]
        .into_iter()
        .zip(s.dac_hold())
        .enumerate()
    {
        let meaning = match hold {
            Some(0x03) => "DAC held on (vendor)",
            Some(_) => "DAC may idle off",
            None => "UNRECOGNISED",
        };
        let label = format!("${addr:04X} DAC hold {}:", ["A", "B"][i]);
        println!("{label:<22}{}  {meaning}", hex(&s.dac_routines[i]));
    }
    let window = match s.parts() {
        Some([a, b]) if a == b => format!("{} routine", a.name()),
        Some([a, b]) => format!("half-changed: {} / {}", a.name(), b.name()),
        None => "UNRECOGNISED".into(),
    };
    println!("$1EFC window:         {window}");
    match s.mic_boost() {
        Some(n) => println!("$1017 MIC_CONTROL:    {:#04x}  boost {n}", s.mic_control),
        None => println!(
            "$1017 MIC_CONTROL:    {:#04x}  outside 0x38..0x3F",
            s.mic_control
        ),
    }
    println!(
        "$0F18 DAC enable:     {:#04x}  L {} / R {}",
        s.dac_enable,
        on_off(s.dac_enable & 1),
        on_off(s.dac_enable & 2)
    );
    let hp = match s.hp_driver {
        0x00 => "active",
        0x33 => "parked",
        0x77 => "clamped",
        _ => "other",
    };
    println!("$0F1A headphone drv:  {:#04x}  {hp}", s.hp_driver);
    println!("$0F1B:                {:#04x}", s.reg_0f1b);
    println!("$0F2C:                {:#04x}", s.reg_0f2c);
    println!(
        "$0F3E references:     {:#04x}  {}",
        s.refs,
        on_off(s.refs & cxtune::patches::REFS_ON)
    );
    println!(
        "$1151 DSP enable:     {:#04x}  {}",
        s.dsp_enable,
        dsp_text(s.dsp_enable)
    );
    println!(
        "$1276 sidetone level: {:#06x}  {}",
        s.sidetone_level,
        level_text(s.sidetone_level)
    );
    println!(
        "$00A4 applied:        {:#04x}  noise reduction {}, AGC {} (the ROM evaluates $1151 at a \
         capture stream start or NEWC)",
        s.appliers,
        applied_text(s.appliers & APPLIED_NR),
        applied_text(s.appliers & APPLIED_AGC)
    );
    println!(
        "$00A8 mixer:          {:#04x}  sidetone {}",
        s.mixer,
        if s.mixer & MIXER_SIDETONE != 0 {
            "running"
        } else {
            "not running"
        }
    );
    println!(
        "$13DC capture stream: {:#04x}  {}",
        s.mic_stream,
        if s.mic_stream != 0 { "open" } else { "closed" }
    );
}

fn on_off(bit: u8) -> &'static str {
    if bit != 0 { "on" } else { "off" }
}

fn applied_text(bit: u8) -> &'static str {
    if bit != 0 { "applied" } else { "not applied" }
}

fn dsp_text(bits: u8) -> String {
    let text = format!(
        "mic noise reduction {}, AGC {}, sidetone {}",
        on_off(bits & DSP_NR),
        on_off(bits & DSP_AGC),
        on_off(bits & DSP_SIDETONE)
    );
    let other = bits & !(DSP_NR | DSP_AGC | DSP_SIDETONE);
    if other == 0 {
        text
    } else {
        format!("{text}; other bits {other:#04x} are set, cxtune will not change this byte")
    }
}

fn level_text(level: u16) -> String {
    if (SIDETONE_MIN..=SIDETONE_MAX).contains(&level) {
        format!("{level} of {SIDETONE_MAX}")
    } else {
        format!("outside {SIDETONE_MIN}..={SIDETONE_MAX}, cxtune will not change it")
    }
}

fn apply(name: &str) -> Result<()> {
    let target = Mode::parse(name).ok_or_else(|| {
        Error::Config(format!(
            "unknown mode '{name}' (stock | quiet | balanced | min-hiss)"
        ))
    })?;
    let mut cfg = config::load()?;
    let n = settings::set_mode(&mut open()?, target, cfg.min_hiss_ms, audio::open_silence)?;
    println!("mode '{}' applied ({n} writes)", target.name());
    cfg.mode = Some(target);
    config::save(&cfg)
}

fn tune(arg: &str) -> Result<()> {
    let ms: u16 = arg
        .parse()
        .map_err(|_| Error::Config(format!("settle time must be a number of ms, got '{arg}'")))?;
    let mut cfg = config::load()?;
    let written = mode::tune_min_hiss(&mut open()?, ms)?;
    println!(
        "min-hiss settle time {ms} ms{}",
        if written { " set" } else { " already set" }
    );
    cfg.min_hiss_ms = Some(ms);
    config::save(&cfg)
}

fn mic_boost(arg: &str) -> Result<()> {
    let step: u8 = arg
        .parse()
        .map_err(|_| Error::Config(format!("mic boost must be 0-7, got '{arg}'")))?;
    let mut cfg = config::load()?;
    let written = mode::set_mic_boost(&mut open()?, step)?;
    println!(
        "mic boost {step}{}",
        if written { " set" } else { " already set" }
    );
    cfg.mic_boost = Some(step);
    config::save(&cfg)
}

fn parse_switch(what: &str, arg: &str) -> Result<bool> {
    match arg {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => Err(Error::Config(format!(
            "{what} must be 'on' or 'off', got '{arg}'"
        ))),
    }
}

fn written_text(written: bool) -> &'static str {
    if written { "set" } else { "already set" }
}

/// Sets one on/off microphone setting on the device and remembers it.
fn switch(
    what: &str,
    arg: &str,
    set: fn(&mut hid::HidTransport, bool) -> Result<dsp::Applied>,
    remember: fn(&mut Config, bool),
) -> Result<()> {
    let on = parse_switch(what, arg)?;
    let mut cfg = config::load()?;
    let applied = set(&mut open()?, on)?;
    println!(
        "{what} {} ({})",
        if on { "on" } else { "off" },
        written_text(applied.writes > 0)
    );
    if let Some(event) = applied.stream_event {
        eprintln!("warning: {event}");
    }
    remember(&mut cfg, on);
    config::save(&cfg)
}

fn sidetone(arg: &str) -> Result<()> {
    let on = parse_switch("sidetone", arg)?;
    let mut cfg = config::load()?;
    let written = dsp::set_sidetone_with_level(&mut open()?, on, cfg.sidetone_level)?;
    println!(
        "sidetone {} ({})",
        if on { "on" } else { "off" },
        written_text(written > 0)
    );
    cfg.sidetone = Some(on);
    config::save(&cfg)
}

fn sidetone_level(arg: &str) -> Result<()> {
    let level: u16 = arg.parse().map_err(|_| {
        Error::Config(format!(
            "sidetone level must be {SIDETONE_MIN}-{SIDETONE_MAX}, got '{arg}'"
        ))
    })?;
    let mut cfg = config::load()?;
    let written = dsp::set_sidetone_level(&mut open()?, level)? > 0;
    println!("sidetone level {level} ({})", written_text(written));
    cfg.sidetone_level = Some(level);
    config::save(&cfg)
}

fn watch() -> Result<()> {
    let engine = Engine::start(report)?;
    println!("watching for the dongle (Ctrl+C to stop)");
    while engine.is_running() {
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    Err(Error::Device("the engine stopped".into()))
}

fn report(event: Event) {
    match event {
        Event::State(state) => println!("{}", describe(&state)),
        Event::Warning(message) => eprintln!("warning: {message}"),
        Event::Error(failure) => eprintln!("error ({}): {}", failure.kind, failure.message),
    }
}

fn describe(state: &Snapshot) -> String {
    let phase = match (state.phase, &state.device) {
        (Phase::Stopped, _) => "engine stopped",
        (Phase::Disconnected, _) => "dongle not connected",
        (Phase::Waiting, _) => "dongle connected, waiting for its firmware",
        (Phase::Applying, _) => "applying the saved settings",
        (Phase::Connected, None) => "dongle connected",
        (Phase::Connected, Some(_)) if state.in_sync() => "dongle holds the saved settings",
        (Phase::Connected, Some(_)) => "dongle differs from the saved settings",
    };
    match &state.device {
        Some(device) => format!("{phase}: {device}"),
        None => phase.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use cxtune::engine::Readback;
    use cxtune::patches::{BALANCED_WINDOW, SIDETONE_DEFAULT};

    use super::*;

    fn balanced(rom: [u8; ROM_ID.len()]) -> State {
        State {
            rom,
            hook: [0x4C, 0xFC, 0x1E, 0x4C],
            dac_routines: [[0xAD, 0x18, 0x0F, 0x29, 0xDF, 0x09, 0x00, 0x8D, 0x18, 0x0F]; 2],
            window: BALANCED_WINDOW,
            mic_control: 0x3E,
            dac_enable: 0,
            hp_driver: 0x77,
            reg_0f1b: 0x33,
            reg_0f2c: 0x60,
            refs: 0,
            dsp_enable: 0x04,
            sidetone_level: SIDETONE_DEFAULT,
            appliers: 0,
            mixer: 0,
            mic_stream: 0,
        }
    }

    #[test]
    fn status_names_a_rom_identity_mismatch() {
        let known = balanced(ROM_ID);
        assert_eq!(mode_text(&known), "balanced");
        assert!(rom_note(&known.rom).contains("matches"));

        // the same bytes everywhere else, a different firmware version or chip id
        for i in 0..ROM_ID.len() {
            let mut rom = ROM_ID;
            rom[i] ^= 0x01;
            let other = balanced(rom);
            assert!(mode_text(&other).contains("ROM identity"), "{rom:02x?}");
            let note = rom_note(&other.rom);
            assert!(note.contains("DOES NOT MATCH"), "{note}");
            assert!(note.contains(&hex(&ROM_ID)), "{note}");
        }
    }

    #[test]
    fn watch_describes_each_state_in_one_line() {
        let readback = Readback::of(&balanced(ROM_ID));
        let state = |phase, device: Option<Readback>, mode| Snapshot {
            phase,
            settings: Config {
                mode,
                ..Config::default()
            },
            settings_saved: true,
            save_error: None,
            device,
            error: None,
        };
        assert_eq!(
            describe(&state(Phase::Disconnected, None, None)),
            "dongle not connected"
        );
        assert_eq!(
            describe(&state(Phase::Stopped, None, None)),
            "engine stopped"
        );
        assert_eq!(
            describe(&state(Phase::Waiting, None, None)),
            "dongle connected, waiting for its firmware"
        );
        assert_eq!(
            describe(&state(Phase::Applying, Some(readback.clone()), None)),
            format!("applying the saved settings: {readback}")
        );
        assert_eq!(
            describe(&state(
                Phase::Connected,
                Some(readback.clone()),
                Some(Mode::Balanced)
            )),
            format!("dongle holds the saved settings: {readback}")
        );
        assert_eq!(
            describe(&state(
                Phase::Connected,
                Some(readback.clone()),
                Some(Mode::Quiet)
            )),
            format!("dongle differs from the saved settings: {readback}")
        );
    }

    #[test]
    fn dsp_enable_is_described_bit_by_bit() {
        assert_eq!(
            dsp_text(0x04),
            "mic noise reduction on, AGC off, sidetone off"
        );
        assert_eq!(
            dsp_text(0x90),
            "mic noise reduction off, AGC on, sidetone on"
        );
        assert!(
            dsp_text(0x0C).contains("other bits 0x08"),
            "{}",
            dsp_text(0x0C)
        );
    }

    #[test]
    fn the_sidetone_level_is_judged_against_the_accepted_range() {
        assert_eq!(level_text(947), "947 of 4096");
        for bad in [0, 4097, 0x80B3] {
            assert!(level_text(bad).contains("outside"), "{bad}");
        }
    }

    #[test]
    fn switches_are_exactly_on_or_off() {
        assert!(parse_switch("x", "on").unwrap());
        assert!(!parse_switch("x", "off").unwrap());
        for bad in ["", "On", "true", "1", "yes"] {
            assert!(
                matches!(parse_switch("x", bad), Err(Error::Config(_))),
                "{bad}"
            );
        }
    }
}
