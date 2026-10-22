//! Reads or changes the volume of the dongle's audio endpoints, to check `cxtune::volume`
//! against the Windows mixer.
//!
//! ```text
//! cargo run --example volume -- get
//! cargo run --example volume -- set <playback|capture> <0-100>
//! cargo run --example volume -- mute <playback|capture> <on|off>
//! ```

use cxtune::volume::{Side, Volume};

fn side(text: Option<&str>) -> Side {
    match text {
        Some("playback") => Side::Playback,
        Some("capture") => Side::Capture,
        _ => usage(),
    }
}

fn usage() -> ! {
    eprintln!(
        "usage: volume get | set <playback|capture> <0-100> | mute <playback|capture> <on|off>"
    );
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut volume = Volume::open().expect("COM");
    match args.first().map(String::as_str) {
        Some("get") => {
            for (name, side) in [("playback", Side::Playback), ("capture", Side::Capture)] {
                match volume.level(side) {
                    Some(level) => println!("{name}: {}% muted={}", level.percent, level.muted),
                    None => println!("{name}: no endpoint"),
                }
            }
        }
        Some("set") => {
            let percent = args
                .get(2)
                .and_then(|p| p.parse().ok())
                .unwrap_or_else(|| usage());
            volume
                .set_percent(side(args.get(1).map(String::as_str)), percent)
                .expect("set the volume");
        }
        Some("mute") => {
            let muted = match args.get(2).map(String::as_str) {
                Some("on") => true,
                Some("off") => false,
                _ => usage(),
            };
            volume
                .set_muted(side(args.get(1).map(String::as_str)), muted)
                .expect("set the mute switch");
        }
        _ => usage(),
    }
}
