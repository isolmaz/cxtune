//! Traces the codec's playback state registers while a stream of digital silence is opened and
//! closed on the dongle's output. Reads only; needs the dongle.
//!
//! usage: cargo run --release --example trace -- <open_s> <close_s> <total_s>
//!
//! Every change of ($0F18, $0F1A, $0F1B, $0F15/$0F16 mute bit, $0F3E) is printed with its time
//! in ms. The firmware notices a closed stream about 15-20 s later, so `total_s` should
//! leave that much time after `close_s`.

use std::thread::sleep;
use std::time::{Duration, Instant};

use cxtune::protocol::read_mem;
use cxtune::{Error, Result, audio, hid};

fn secs(arg: Option<String>, default: f64) -> Result<Duration> {
    match arg {
        None => Ok(Duration::from_secs_f64(default)),
        Some(s) => s
            .parse()
            .map(Duration::from_secs_f64)
            .map_err(|_| Error::Config(format!("not a number of seconds: {s}"))),
    }
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let open_at = secs(args.next(), 2.0)?;
    let close_at = secs(args.next(), 7.0)?;
    let total = secs(args.next(), 40.0)?;

    let mut dev = hid::open(&hid::api()?)?;
    let start = Instant::now();
    let mut stream = None;
    let mut last = None;
    while start.elapsed() < total {
        let now = start.elapsed();
        if stream.is_none() && now >= open_at && now < close_at {
            stream = Some(audio::open_silence()?);
            println!("{:>7} ms  stream opened", start.elapsed().as_millis());
        } else if stream.is_some() && now >= close_at {
            stream = None;
            println!("{:>7} ms  stream closed", start.elapsed().as_millis());
        }
        let gains = read_mem(&mut dev, 0x0F15, 2)?;
        let enable = read_mem(&mut dev, 0x0F18, 1)?[0];
        let driver = read_mem(&mut dev, 0x0F1A, 2)?;
        let refs = read_mem(&mut dev, 0x0F3E, 1)?[0];
        let state = (
            enable,
            driver[0],
            driver[1],
            gains[0] >> 7,
            gains[1] >> 7,
            refs,
        );
        if last != Some(state) {
            println!(
                "{:>7} ms  $0F18={:02X} $0F1A={:02X} $0F1B={:02X} mute L/R={}/{} $0F3E={:02X}",
                now.as_millis(),
                state.0,
                state.1,
                state.2,
                state.3,
                state.4,
                state.5
            );
            last = Some(state);
        }
        sleep(Duration::from_millis(5));
    }
    Ok(())
}
