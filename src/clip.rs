//! Mono clips for the test helpers: the test tone, padding in front of and behind a clip, and the
//! peak limit that keeps a playback from being louder than the headphones should get.
//!
//! The samples are 16-bit at [`PLAY_RATE`], as [`crate::audio::play`] takes them.

use crate::audio::PLAY_RATE;

/// Frequency of the test tone.
pub const TONE_HZ: f64 = 1000.0;
/// Peak level of the test tone. Low on purpose: in-ear monitors are sensitive, and what the click
/// test is for is the click, not the tone.
pub const TONE_DBFS: f64 = -40.0;
/// Length of the test tone.
pub const TONE_MS: u32 = 300;
/// Length of the fade at each end of the test tone, so that the tone itself does not click.
pub const TONE_FADE_MS: u32 = 10;
/// The highest peak a recording is played back at. A louder one is scaled down as a whole, so a
/// clap close to the microphone does not reach the headphones at full scale.
pub const PLAYBACK_PEAK_DBFS: f64 = -12.0;

fn frames(ms: u32) -> usize {
    (u64::from(PLAY_RATE) * u64::from(ms) / 1000) as usize
}

/// A sine of `freq_hz` with a peak of `peak_dbfs`, lasting `ms`, with a raised-cosine fade of
/// `fade_ms` at both ends.
pub fn tone(freq_hz: f64, peak_dbfs: f64, ms: u32, fade_ms: u32) -> Vec<i16> {
    let n = frames(ms);
    let fade = frames(fade_ms).min(n / 2);
    let amplitude = 10f64.powf(peak_dbfs / 20.0) * f64::from(i16::MAX);
    (0..n)
        .map(|i| {
            let edge = i.min(n - 1 - i);
            let gain = if edge < fade {
                0.5 - 0.5 * (std::f64::consts::PI * edge as f64 / fade as f64).cos()
            } else {
                1.0
            };
            let phase = 2.0 * std::f64::consts::PI * freq_hz * i as f64 / f64::from(PLAY_RATE);
            (amplitude * gain * phase.sin()).round() as i16
        })
        .collect()
}

/// The test tone: [`TONE_HZ`] at [`TONE_DBFS`] for [`TONE_MS`].
pub fn test_tone() -> Vec<i16> {
    tone(TONE_HZ, TONE_DBFS, TONE_MS, TONE_FADE_MS)
}

/// `clip` with `lead_ms` of silence in front and `tail_ms` of silence behind.
pub fn pad(clip: &[i16], lead_ms: u32, tail_ms: u32) -> Vec<i16> {
    let mut out = vec![0; frames(lead_ms)];
    out.extend_from_slice(clip);
    out.resize(out.len() + frames(tail_ms), 0);
    out
}

/// Scales `clip` down as a whole if its peak is above `max_dbfs`; a quieter clip is not changed.
pub fn limit_peak(clip: &mut [i16], max_dbfs: f64) {
    let peak = clip
        .iter()
        .map(|&s| i32::from(s).unsigned_abs())
        .max()
        .unwrap_or(0);
    let allowed = 10f64.powf(max_dbfs / 20.0) * 32768.0;
    if f64::from(peak) > allowed {
        let scale = allowed / f64::from(peak);
        for sample in clip {
            *sample = (f64::from(*sample) * scale).round() as i16;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::Level;

    #[test]
    fn the_test_tone_is_a_300_ms_tone_40_db_below_full_scale() {
        let clip = test_tone();
        assert_eq!(clip.len(), 14_400);
        let level = Level::of(&clip);
        assert!((level.peak_dbfs + 40.0).abs() < 0.2, "{level:?}");
        // a sine's RMS is 3 dB under its peak, and the fades take a little more
        assert!(
            level.rms_dbfs < level.peak_dbfs - 3.0 && level.rms_dbfs > level.peak_dbfs - 4.0,
            "{level:?}"
        );
    }

    #[test]
    fn the_tone_fades_in_and_out_instead_of_starting_at_full_level() {
        let clip = test_tone();
        let fade = frames(TONE_FADE_MS);
        assert_eq!(clip[0], 0);
        assert_eq!(*clip.last().unwrap(), 0);
        let loudest = |part: &[i16]| part.iter().map(|s| s.unsigned_abs()).max().unwrap();
        let body = loudest(&clip[fade..clip.len() - fade]);
        // the first and last millisecond are far below the body, and the body is at full level
        assert!(loudest(&clip[..48]) * 8 < body, "{}", loudest(&clip[..48]));
        assert!(
            loudest(&clip[clip.len() - 48..]) * 8 < body,
            "{}",
            loudest(&clip[clip.len() - 48..])
        );
        assert!(loudest(&clip) <= body + 1);
    }

    #[test]
    fn the_tone_has_the_requested_frequency() {
        let clip = tone(1000.0, -20.0, 100, 5);
        // 1 kHz at 48 kHz: 48 samples per period, so 100 ms hold 100 rising zero crossings
        let rising = clip.windows(2).filter(|w| w[0] <= 0 && w[1] > 0).count();
        assert!((98..=101).contains(&rising), "{rising}");
    }

    #[test]
    fn a_clip_is_padded_with_silence_on_both_sides() {
        let padded = pad(&[7, 8, 9], 10, 20);
        assert_eq!(padded.len(), 480 + 3 + 960);
        assert!(padded[..480].iter().all(|&s| s == 0));
        assert_eq!(&padded[480..483], &[7, 8, 9]);
        assert!(padded[483..].iter().all(|&s| s == 0));
        assert_eq!(pad(&[], 0, 0), Vec::<i16>::new());
    }

    #[test]
    fn a_loud_clip_is_scaled_down_as_a_whole_and_a_quiet_one_is_left_alone() {
        let mut loud = vec![1000, -32768, 16384, 0];
        limit_peak(&mut loud, -12.0);
        let peak = Level::of(&loud).peak_dbfs;
        assert!((peak + 12.0).abs() < 0.01, "{peak}");
        // the shape is kept: the sample that was half the peak is still about half
        assert!((i32::from(loud[2]) * 2 - i32::from(loud[1]).abs()).abs() <= 2);
        assert_eq!(loud[3], 0);

        let mut quiet = vec![100, -200, 300];
        limit_peak(&mut quiet, -12.0);
        assert_eq!(quiet, [100, -200, 300]);

        let mut none: Vec<i16> = Vec::new();
        limit_peak(&mut none, -12.0);
        assert!(none.is_empty());
    }

    #[test]
    fn a_clip_exactly_at_the_limit_is_not_changed() {
        let limit = (10f64.powf(-12.0 / 20.0) * 32768.0) as i16 - 1;
        let mut clip = vec![limit, -limit];
        limit_peak(&mut clip, -12.0);
        assert_eq!(clip, [limit, -limit]);
    }
}
