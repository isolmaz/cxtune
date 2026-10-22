use std::process::ExitCode;
use std::thread::sleep;
use std::time::{Duration, Instant};

use cxtune::config::{self, Config};
use cxtune::mode::{self, Mode, State};
use cxtune::protocol::Transport;
use cxtune::{Error, Result, audio, hex, hid};

const USAGE: &str = "\
usage: cxtune <command>

  status               read and show the device state (read-only)
  apply <mode>         apply an output mode and remember it:
                       stock | quiet | balanced | min-hiss
  tune <ms>            set the min-hiss settle time (20-2000 in steps of 10) and remember it
  mic-boost <0-7>      set analog mic boost ($1017 = 0x38 + n) and remember it
  watch                keep the remembered settings applied across replugs

Changes live in volatile RAM only; replugging the dongle restores the vendor patch.";

/// How often `watch` checks whether the dongle is present (enumeration only, no device I/O).
const POLL: Duration = Duration::from_secs(1);
/// Wait after the dongle appears so its firmware can load the patch from EEPROM.
const SETTLE: Duration = Duration::from_secs(2);
/// While connected, re-verify the state this often in case a USB reset was missed.
const VERIFY: Duration = Duration::from_secs(60);
const RETRY: Duration = Duration::from_secs(5);

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match args.as_slice() {
        ["status"] => status(),
        ["apply", name] => apply(name),
        ["tune", ms] => tune(ms),
        ["mic-boost", n] => mic_boost(n),
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

fn open() -> Result<hid::HidTransport> {
    hid::open(&hid::api()?)
}

fn status() -> Result<()> {
    let s = mode::read_state(&mut open()?)?;
    print_state(&s);
    Ok(())
}

fn print_state(s: &State) {
    let mode = s.mode().map_or("unrecognised or half-changed", Mode::name);
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
}

fn on_off(bit: u8) -> &'static str {
    if bit != 0 { "on" } else { "off" }
}

/// Applies a mode, then the min-hiss settle time if one is given and the mode is min-hiss.
fn set_mode<T: Transport>(dev: &mut T, target: Mode, min_hiss_ms: Option<u16>) -> Result<usize> {
    let mut n = mode::apply_mode(dev, target, audio::open_silence)?;
    if target == Mode::MinHiss
        && let Some(ms) = min_hiss_ms
    {
        n += usize::from(mode::tune_min_hiss(dev, ms)?);
    }
    Ok(n)
}

fn apply(name: &str) -> Result<()> {
    let target = Mode::parse(name).ok_or_else(|| {
        Error::Config(format!(
            "unknown mode '{name}' (stock | quiet | balanced | min-hiss)"
        ))
    })?;
    let mut cfg = config::load()?;
    let n = set_mode(&mut open()?, target, cfg.min_hiss_ms)?;
    println!("mode '{}' applied ({n} writes)", target.name());
    cfg.mode = Some(target);
    config::save(&cfg)
}

fn tune(arg: &str) -> Result<()> {
    let ms: u16 = arg
        .parse()
        .map_err(|_| Error::Config(format!("settle time must be a number of ms, got '{arg}'")))?;
    let written = mode::tune_min_hiss(&mut open()?, ms)?;
    println!(
        "min-hiss settle time {ms} ms{}",
        if written { " set" } else { " already set" }
    );
    let mut cfg = config::load()?;
    cfg.min_hiss_ms = Some(ms);
    config::save(&cfg)
}

fn mic_boost(arg: &str) -> Result<()> {
    let step: u8 = arg
        .parse()
        .map_err(|_| Error::Config(format!("mic boost must be 0-7, got '{arg}'")))?;
    let mut dev = open()?;
    let written = mode::set_mic_boost(&mut dev, step)?;
    println!(
        "mic boost {step}{}",
        if written { " set" } else { " already set" }
    );
    let mut cfg = config::load()?;
    cfg.mic_boost = Some(step);
    config::save(&cfg)
}

/// Applies the saved settings. Idempotent: writes nothing if the device already matches.
fn ensure<T: Transport>(dev: &mut T, cfg: &Config) -> Result<usize> {
    let mut n = match cfg.mode {
        Some(m) => set_mode(dev, m, cfg.min_hiss_ms)?,
        None => 0,
    };
    if let Some(step) = cfg.mic_boost {
        n += usize::from(mode::set_mic_boost(dev, step)?);
    }
    Ok(n)
}

fn watch() -> Result<()> {
    let mut api = hid::api()?;
    println!("watching for the dongle (Ctrl+C to stop)");
    let mut present = false;
    let mut next_check = Instant::now();
    let mut last_error: Option<String> = None;
    loop {
        if let Err(e) = api.refresh_devices() {
            eprintln!("enumeration failed: {e}");
        }
        let found = hid::find(&api).is_some();
        if found && !present {
            println!("dongle connected");
            next_check = Instant::now() + SETTLE;
            last_error = None;
        } else if !found && present {
            println!("dongle disconnected");
        }
        present = found;

        if present && Instant::now() >= next_check {
            let result = config::load().and_then(|cfg| {
                if cfg == Config::default() {
                    return Err(Error::Config(
                        "nothing selected yet; run `cxtune apply <mode>` first".into(),
                    ));
                }
                ensure(&mut hid::open(&api)?, &cfg)
            });
            match result {
                Ok(n) => {
                    if n > 0 {
                        println!("settings re-applied ({n} writes)");
                    }
                    last_error = None;
                    next_check = Instant::now() + VERIFY;
                }
                Err(e @ Error::Readback { .. }) => return Err(e),
                Err(e) => {
                    let msg = e.to_string();
                    if last_error.as_deref() != Some(msg.as_str()) {
                        eprintln!("error: {msg}");
                    }
                    last_error = Some(msg);
                    next_check = Instant::now() + RETRY;
                }
            }
        }
        sleep(POLL);
    }
}
