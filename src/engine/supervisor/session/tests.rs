use std::sync::atomic::Ordering;

use super::*;
use crate::config::Config;
use crate::engine::supervisor::SETTLE;
use crate::engine::testing::Rig;
use crate::engine::types::{Command, Phase};
use crate::fake::FakeDevice;

fn rig_with(mode: Mode, noise_reduction: bool) -> Rig {
    let cfg = Config {
        mode: Some(mode),
        mic_nr: Some(noise_reduction),
        ..Config::default()
    };
    let mut rig = Rig::with_device(FakeDevice::in_mode(mode), cfg, true);
    rig.run(5.0);
    assert_eq!(rig.phase(), Phase::Connected);
    assert!(rig.snapshot().in_sync(), "{:?}", rig.snapshot());
    rig
}

fn rig_in(mode: Mode) -> Rig {
    rig_with(mode, true)
}

fn test(rig: &mut Rig, request: TestRequest) {
    rig.send(Command::Test(request));
}

fn mic_open(rig: &Rig) -> bool {
    rig.dev.byte(0x13DC) != 0
}

fn playing(rig: &Rig) -> bool {
    rig.dev.byte(0x13DD) != 0
}

fn levels(rig: &Rig) -> Vec<Level> {
    rig.test_events()
        .into_iter()
        .filter_map(|e| match e {
            TestEvent::Level(level) => Some(level),
            _ => None,
        })
        .collect()
}

fn outcomes(rig: &Rig) -> Vec<TestOutcome> {
    rig.test_events()
        .into_iter()
        .filter_map(|e| match e {
            TestEvent::Done(outcome) => Some(outcome),
            _ => None,
        })
        .collect()
}

fn failures(rig: &Rig) -> Vec<(Option<TestKind>, Failure)> {
    rig.test_events()
        .into_iter()
        .filter_map(|e| match e {
            TestEvent::Failed { kind, failure } => Some((kind, failure)),
            _ => None,
        })
        .collect()
}

/// The steps a test went through, each once, in order.
fn steps(rig: &Rig) -> Vec<TestStep> {
    let mut steps: Vec<TestStep> = Vec::new();
    for event in rig.test_events() {
        if let TestEvent::Progress(progress) = event
            && steps.last() != Some(&progress.step)
        {
            steps.push(progress.step);
        }
    }
    steps
}

fn over(rig: &Rig) -> usize {
    rig.test_events()
        .iter()
        .filter(|e| matches!(e, TestEvent::Done(_) | TestEvent::Failed { .. }))
        .count()
}

/// Runs until the next test ends or fails and returns the seconds that took.
fn until_over(rig: &mut Rig, limit: f64) -> f64 {
    let before = over(rig);
    let mut waited = 0.0;
    while over(rig) == before {
        assert!(waited < limit, "the test did not end within {limit} s");
        rig.run(0.05);
        waited += 0.05;
    }
    waited
}

fn near(got: f32, want: f32) {
    assert!((got - want).abs() < 0.1, "{got} is not near {want}");
}

fn played(rig: &Rig) -> Vec<Vec<i16>> {
    rig.played.lock().unwrap().clone()
}

fn peak(clip: &[i16]) -> u32 {
    clip.iter()
        .map(|&s| i32::from(s).unsigned_abs())
        .max()
        .unwrap()
}

fn nr_writes(rig: &Rig, from: usize) -> Vec<u8> {
    rig.dev.writes()[from..]
        .iter()
        .filter(|(addr, _)| *addr == 0x1151)
        .map(|(_, data)| data[0])
        .collect()
}

#[test]
fn turning_the_meter_off_does_not_take_the_microphone_from_a_running_test() {
    for request in [
        TestRequest::NoiseFloor { seconds: 1 },
        TestRequest::Record { seconds: 1 },
        TestRequest::NoiseReductionAb { seconds: 1 },
    ] {
        let mut rig = rig_in(Mode::Quiet);
        test(&mut rig, TestRequest::Meter(true));
        rig.run(0.5);
        test(&mut rig, request);
        rig.run(0.5);
        test(&mut rig, TestRequest::Meter(false));
        assert!(mic_open(&rig), "{request:?}");
        until_over(&mut rig, 20.0);
        assert!(failures(&rig).is_empty(), "{request:?}");
        assert_eq!(outcomes(&rig).len(), 1, "{request:?}");
        assert!(!mic_open(&rig), "{request:?}");
    }
}

#[test]
fn the_meter_cannot_be_turned_on_during_a_test_that_needs_the_output_at_rest() {
    for mode in [Mode::Balanced, Mode::Quiet] {
        for (kind, request) in [
            (TestKind::ClickTest, TestRequest::ClickTest),
            (TestKind::NoiseListen, TestRequest::NoiseListen),
            (TestKind::PlayRecording, TestRequest::PlayRecording),
        ] {
            let mut rig = rig_in(mode);
            if kind == TestKind::PlayRecording {
                test(&mut rig, TestRequest::Record { seconds: 1 });
                until_over(&mut rig, 10.0);
            }
            let refused = failures(&rig).len();
            let done = outcomes(&rig).len();
            test(&mut rig, request);
            // in the same pass as the request, and again while the test is under way
            test(&mut rig, TestRequest::Meter(true));
            rig.run(0.2);
            if outcomes(&rig).len() == done {
                test(&mut rig, TestRequest::Meter(true));
            }
            let mut waited = 0.0;
            while outcomes(&rig).len() == done {
                rig.run(0.05);
                waited += 0.05;
                assert!(waited < 20.0, "{mode:?} {kind:?} did not end");
                assert!(!mic_open(&rig), "{mode:?} {kind:?}: the microphone is open");
                assert!(!rig.snapshot().test.meter, "{mode:?} {kind:?}");
            }
            let got = failures(&rig);
            assert!(
                got.len() > refused,
                "{mode:?} {kind:?}: nothing was refused"
            );
            for (failed_kind, failure) in &got[refused..] {
                assert_eq!(*failed_kind, None, "{mode:?} {kind:?}");
                assert_eq!(failure.kind, ErrorKind::Invalid, "{mode:?} {kind:?}");
                assert!(
                    failure.message.contains("output test"),
                    "{}",
                    failure.message
                );
            }
            let outcome = outcomes(&rig).last().copied();
            match kind {
                TestKind::ClickTest => {
                    assert!(
                        matches!(outcome, Some(TestOutcome::ClickTest { .. })),
                        "{mode:?}"
                    );
                    assert_eq!(played(&rig).len(), 1, "{mode:?}");
                }
                TestKind::NoiseListen => {
                    assert!(
                        matches!(outcome, Some(TestOutcome::NoiseListen { .. })),
                        "{mode:?}"
                    );
                }
                _ => assert_eq!(outcome, Some(TestOutcome::Played), "{mode:?}"),
            }
            assert!(!mic_open(&rig));
            assert!(!rig.snapshot().test.meter);
        }
    }
}

#[test]
fn the_meter_can_be_turned_on_during_a_measurement_and_keeps_the_microphone_after_it() {
    let mut rig = rig_in(Mode::Quiet);
    test(&mut rig, TestRequest::NoiseFloor { seconds: 1 });
    rig.run(0.5);
    test(&mut rig, TestRequest::Meter(true));
    rig.run(0.1);
    assert!(rig.snapshot().test.meter);
    until_over(&mut rig, 20.0);
    assert!(failures(&rig).is_empty());
    assert!(mic_open(&rig));
}

#[test]
fn the_meter_opens_the_microphone_reports_its_level_and_closes_it_when_told_to() {
    let mut rig = rig_in(Mode::Quiet);
    assert!(!mic_open(&rig));
    test(&mut rig, TestRequest::Meter(true));
    rig.run(1.0);
    assert!(mic_open(&rig));
    assert!(rig.snapshot().test.meter);
    let seen = levels(&rig);
    assert_eq!(seen.len(), 20, "one level per 50 ms");
    // what the codec delivers while it starts up is reported as it is
    assert!(seen.iter().all(|l| (l.rms_dbfs + 20.0).abs() < 0.1));
    rig.run(3.0);
    near(levels(&rig).last().unwrap().rms_dbfs, -60.0);

    test(&mut rig, TestRequest::Meter(false));
    assert!(!mic_open(&rig), "closed at once, not left to a timer");
    rig.run(0.1);
    assert!(!rig.snapshot().test.meter);
    let count = levels(&rig).len();
    rig.run(2.0);
    assert_eq!(levels(&rig).len(), count);
    assert!(failures(&rig).is_empty());
}

#[test]
fn the_noise_floor_is_the_median_of_what_follows_the_warm_up() {
    let mut rig = rig_in(Mode::Quiet);
    test(&mut rig, TestRequest::NoiseFloor { seconds: 2 });
    rig.run(0.2);
    assert!(mic_open(&rig));
    assert_eq!(rig.snapshot().test.running, Some(TestKind::NoiseFloor));

    let waited = until_over(&mut rig, 10.0) + 0.2;
    // the warm-up and the 2 s that were asked for
    assert!(
        (waited - (WARMUP.as_secs_f64() + 2.0)).abs() < 0.15,
        "{waited}"
    );
    let [TestOutcome::NoiseFloor { dbfs }] = outcomes(&rig)[..] else {
        panic!("{:?}", outcomes(&rig));
    };
    // the first 3.2 s of the stream are loud: they are not in the median
    near(dbfs, -60.0);
    assert_eq!(steps(&rig), [TestStep::WarmingUp, TestStep::Measuring]);
    assert!(!mic_open(&rig));
    assert_eq!(rig.snapshot().test, TestState::default());
}

#[test]
fn a_loud_start_up_that_lasts_into_the_measurement_would_show() {
    // the warm-up is fixed: a codec that is still loud after it is measured as it is
    let mut rig = rig_in(Mode::Quiet);
    rig.mic.lock().unwrap().warmup = Duration::from_secs(10);
    test(&mut rig, TestRequest::NoiseFloor { seconds: 2 });
    until_over(&mut rig, 10.0);
    let [TestOutcome::NoiseFloor { dbfs }] = outcomes(&rig)[..] else {
        panic!("{:?}", outcomes(&rig));
    };
    near(dbfs, -20.0);
}

#[test]
fn the_countdown_of_a_step_runs_down() {
    let mut rig = rig_in(Mode::Quiet);
    test(&mut rig, TestRequest::NoiseFloor { seconds: 2 });
    until_over(&mut rig, 10.0);
    let remaining = |step| -> Vec<u32> {
        rig.test_events()
            .into_iter()
            .filter_map(|e| match e {
                TestEvent::Progress(p) if p.step == step && p.kind == TestKind::NoiseFloor => {
                    p.remaining_ms
                }
                _ => None,
            })
            .collect()
    };
    let warm_ms = WARMUP.as_millis() as u32;
    for (step, total) in [(TestStep::WarmingUp, warm_ms), (TestStep::Measuring, 2000)] {
        let left = remaining(step);
        assert!(left.len() >= 5, "{step:?}: {left:?}");
        assert!(left.windows(2).all(|w| w[0] > w[1]), "{step:?}: {left:?}");
        assert!(
            left[0] <= total && left[0] > total - 400,
            "{step:?}: {left:?}"
        );
        assert!(*left.last().unwrap() <= 150, "{step:?}: {left:?}");
    }
    // a report every 100 ms at most, not every 50 ms tick
    let warm = remaining(TestStep::WarmingUp);
    assert!(
        warm.len() <= WARMUP.as_millis() as usize / 100 + 2,
        "{}",
        warm.len()
    );
}

#[test]
fn a_recording_keeps_the_samples_and_plays_back_after_the_mute_delay_of_the_mode() {
    let mut rig = rig_in(Mode::Balanced);
    test(&mut rig, TestRequest::Record { seconds: 2 });
    let waited = until_over(&mut rig, 10.0);
    assert!(
        (waited - (WARMUP.as_secs_f64() + 2.0)).abs() < 0.15,
        "{waited}"
    );
    let [
        TestOutcome::Recorded {
            seconds,
            rms_dbfs,
            peak_dbfs,
        },
    ] = outcomes(&rig)[..]
    else {
        panic!("{:?}", outcomes(&rig));
    };
    near(seconds, 2.0);
    near(rms_dbfs, -60.0);
    near(peak_dbfs, -60.0);
    assert!(rig.snapshot().test.has_recording);
    assert!(!mic_open(&rig));
    assert!(played(&rig).is_empty());

    test(&mut rig, TestRequest::PlayRecording);
    until_over(&mut rig, 10.0);
    assert_eq!(outcomes(&rig).last(), Some(&TestOutcome::Played));
    let clips = played(&rig);
    assert_eq!(clips.len(), 1);
    let clip = &clips[0];
    // 650 ms mute delay of balanced mode and 200 ms margin, 2 s of recording, 300 ms tail
    assert_eq!(clip.len(), 48 * (850 + 2000 + 300));
    assert!(clip[..48 * 850].iter().all(|&s| s == 0));
    assert!(clip[48 * 850..48 * 2850].iter().all(|&s| s != 0));
    assert!(clip[48 * 2850..].iter().all(|&s| s == 0));
    // a recording as quiet as this one is played as it was
    assert_eq!(peak(clip), 33);
    assert!(!playing(&rig), "the stream ends with the clip");
    assert!(rig.snapshot().test.has_recording, "it can be played again");
}

#[test]
fn a_loud_recording_is_played_back_no_louder_than_the_limit() {
    let mut rig = rig_in(Mode::Quiet);
    rig.mic.lock().unwrap().floor_nr_on_dbfs = -3.0;
    test(&mut rig, TestRequest::Record { seconds: 1 });
    until_over(&mut rig, 10.0);
    let [TestOutcome::Recorded { peak_dbfs, .. }] = outcomes(&rig)[..] else {
        panic!("{:?}", outcomes(&rig));
    };
    near(peak_dbfs, -3.0);

    test(&mut rig, TestRequest::PlayRecording);
    until_over(&mut rig, 10.0);
    let limit = 10f64.powf(-12.0 / 20.0) * 32768.0;
    let top = f64::from(peak(&played(&rig)[0]));
    assert!(top <= limit + 1.0, "{top} is above {limit}");
    assert!(top >= limit - 40.0, "{top} is far below {limit}");
}

#[test]
fn a_recording_is_replaced_by_the_next_one_and_there_is_nothing_to_play_without_one() {
    let mut rig = rig_in(Mode::Quiet);
    test(&mut rig, TestRequest::PlayRecording);
    rig.run(0.2);
    let [(Some(TestKind::PlayRecording), failure)] = &failures(&rig)[..] else {
        panic!("{:?}", failures(&rig));
    };
    assert_eq!(failure.kind, ErrorKind::Invalid);
    assert!(played(&rig).is_empty());
    assert!(!rig.snapshot().test.has_recording);

    test(&mut rig, TestRequest::Record { seconds: 1 });
    until_over(&mut rig, 10.0);
    test(&mut rig, TestRequest::Record { seconds: 2 });
    until_over(&mut rig, 10.0);
    test(&mut rig, TestRequest::PlayRecording);
    until_over(&mut rig, 10.0);
    // Quiet: no mute delay, 200 ms margin, the second recording of 2 s, 300 ms tail
    assert_eq!(played(&rig)[0].len(), 48 * (200 + 2000 + 300));
}

#[test]
fn the_click_test_plays_a_low_tone_after_the_mute_delay_of_the_mode() {
    // (mode, clip length in ms): margin 200 ms, tone 300 ms, tail 300 ms and the mute delay
    for (mode, length) in [
        (Mode::Stock, 800),
        (Mode::Quiet, 800),
        (Mode::Balanced, 1450),
        (Mode::MinHiss, 1550),
    ] {
        let mut rig = rig_in(mode);
        test(&mut rig, TestRequest::ClickTest);
        let waited = until_over(&mut rig, 10.0);
        // half a second of rest, then the clip
        let want = 0.5 + length as f64 / 1000.0;
        assert!((waited - want).abs() < 0.2, "{mode:?}: {waited} for {want}");
        let [TestOutcome::ClickTest { waited_ms }] = outcomes(&rig)[..] else {
            panic!("{mode:?}: {:?}", outcomes(&rig));
        };
        assert!(
            waited_ms < 100,
            "{mode:?}: an idle output is not waited for"
        );
        assert_eq!(
            steps(&rig),
            [TestStep::Resting, TestStep::Playing],
            "{mode:?}"
        );

        let clips = played(&rig);
        assert_eq!(clips.len(), 1, "{mode:?}");
        assert_eq!(clips[0].len(), 48 * length, "{mode:?}");
        let lead = clips[0].iter().position(|&s| s != 0).unwrap();
        let mute =
            i32::from(mode == Mode::Balanced) * 650 + i32::from(mode == Mode::MinHiss) * 750 + 200;
        // the tone starts with a fade, so its first non-zero sample is a little after the lead
        assert!(
            (lead as i32 - 48 * mute).abs() < 48 * 5,
            "{mode:?}: tone starts at sample {lead}"
        );
        // low: 40 dB below full scale
        assert!((300..=330).contains(&peak(&clips[0])), "{mode:?}");
        assert!(!playing(&rig), "{mode:?}");
    }
}

#[test]
fn the_tone_starts_only_after_the_output_has_settled_into_idle() {
    let mut rig = rig_in(Mode::Balanced);
    // the driver stays open for a good while after the microphone closes
    rig.dev.hold_driver_after_capture(60);
    test(&mut rig, TestRequest::Meter(true));
    rig.run(1.0);
    test(&mut rig, TestRequest::ClickTest);
    assert!(
        !mic_open(&rig),
        "the click test needs the microphone closed"
    );
    rig.run(0.3);
    assert!(played(&rig).is_empty());
    assert_eq!(steps(&rig).last(), Some(&TestStep::WaitingForIdle));

    until_over(&mut rig, 10.0);
    let [TestOutcome::ClickTest { waited_ms }] = outcomes(&rig)[..] else {
        panic!("{:?}", outcomes(&rig));
    };
    assert!((400..1500).contains(&waited_ms), "{waited_ms}");
    assert_eq!(played(&rig).len(), 1);
    assert_eq!(
        steps(&rig),
        [
            TestStep::WaitingForIdle,
            TestStep::Resting,
            TestStep::Playing
        ]
    );
    assert!(
        !rig.snapshot().test.meter,
        "the meter ends with the click test"
    );
}

#[test]
fn an_output_that_never_becomes_idle_ends_the_wait_and_plays_nothing() {
    for request in [TestRequest::ClickTest, TestRequest::NoiseListen] {
        let mut rig = rig_in(Mode::Balanced);
        // another program holds a playback stream open
        rig.dev.poke(0x13DD, &[1]);
        test(&mut rig, request);
        let waited = until_over(&mut rig, 20.0);
        assert!((waited - 15.0).abs() < 0.2, "{waited}");
        let [(Some(_), failure)] = &failures(&rig)[..] else {
            panic!("{:?}", failures(&rig));
        };
        assert_eq!(failure.kind, ErrorKind::Audio);
        assert!(
            failure.message.contains("did not become idle within 15 s"),
            "{}",
            failure.message
        );
        assert!(
            failure.message.contains("playback 0x01"),
            "{}",
            failure.message
        );
        assert!(played(&rig).is_empty());
        assert_eq!(rig.snapshot().test.running, None);
    }
}

#[test]
fn noise_listening_waits_for_idle_and_leaves_no_stream_open() {
    let mut rig = rig_in(Mode::Balanced);
    rig.dev.hold_driver_after_capture(100);
    test(&mut rig, TestRequest::Meter(true));
    rig.run(0.5);
    let streams = rig.dev.streams_opened();
    test(&mut rig, TestRequest::NoiseListen);
    until_over(&mut rig, 10.0);
    let [TestOutcome::NoiseListen { waited_ms }] = outcomes(&rig)[..] else {
        panic!("{:?}", outcomes(&rig));
    };
    assert!(waited_ms >= 300, "{waited_ms}");
    assert!(!mic_open(&rig));
    assert!(!playing(&rig));
    assert_eq!(rig.dev.streams_opened(), streams);
    assert!(played(&rig).is_empty());
    // the output really is idle now
    assert_eq!(rig.dev.byte(0x0F3E) & 0x02, 0);
    assert_eq!(rig.snapshot().test, TestState::default());
}

fn db_between(rig: &Rig) -> (f32, f32, f32) {
    match outcomes(rig)[..] {
        [
            TestOutcome::NoiseReductionAb {
                with_dbfs,
                without_dbfs,
                reduction_db,
            },
        ] => (with_dbfs, without_dbfs, reduction_db),
        _ => panic!("{:?}", outcomes(rig)),
    }
}

#[test]
fn the_noise_reduction_comparison_measures_both_halves_and_puts_the_setting_back() {
    for original in [true, false] {
        let mut rig = rig_with(Mode::Quiet, original);
        let writes = rig.dev.writes().len();
        test(&mut rig, TestRequest::NoiseReductionAb { seconds: 2 });
        let waited = until_over(&mut rig, 15.0);
        // the warm-up, 2 s with, 1 s to settle and 2 s without
        assert!(
            (waited - (WARMUP.as_secs_f64() + 5.0)).abs() < 0.2,
            "{waited}"
        );

        let (with, without, reduction) = db_between(&rig);
        near(with, -60.0);
        near(without, -48.0);
        near(reduction, 12.0);
        assert_eq!(
            steps(&rig),
            [
                TestStep::WarmingUp,
                TestStep::MeasuringWithNoiseReduction,
                TestStep::SwitchingNoiseReduction,
                TestStep::MeasuringWithoutNoiseReduction
            ]
        );
        // the user's setting is on the dongle again, and the chosen setting never changed
        assert_eq!(rig.dev.byte(0x1151) & 0x04 != 0, original);
        assert_eq!(rig.snapshot().settings.mic_nr, Some(original));
        // on first, off for the second half, then back
        let expected: &[u8] = if original {
            &[0x00, 0x04]
        } else {
            &[0x04, 0x00]
        };
        assert_eq!(nr_writes(&rig, writes), expected, "{original}");
        assert!(!mic_open(&rig));
        assert!(rig.warnings().is_empty(), "{:?}", rig.warnings());
        rig.send(Command::Refresh);
        rig.run(0.5);
        assert!(rig.snapshot().in_sync(), "{:?}", rig.snapshot());
        assert!(rig.errors().is_empty());
    }
}

#[test]
fn a_comparison_started_with_the_meter_on_lets_the_level_settle_after_the_first_switch() {
    for original in [true, false] {
        let mut rig = rig_with(Mode::Quiet, original);
        test(&mut rig, TestRequest::Meter(true));
        rig.run(WARMUP.as_secs_f64() + 1.0);
        test(&mut rig, TestRequest::NoiseReductionAb { seconds: 1 });
        let waited = until_over(&mut rig, 15.0);
        // the first settle, 1 s with, the second settle and 1 s without
        assert!(
            (waited - (2.0 * SWITCH_SETTLE.as_secs_f64() + 2.0)).abs() < 0.2,
            "{original}: {waited}"
        );
        assert_eq!(rig.dev.byte(0x1151) & 0x04 != 0, original);
        assert!(mic_open(&rig), "the meter keeps the microphone");
    }
}

#[test]
fn a_comparison_that_is_cancelled_puts_the_setting_back() {
    let mut rig = rig_in(Mode::Quiet);
    test(&mut rig, TestRequest::NoiseReductionAb { seconds: 5 });
    // into the second half: the warm-up, 5 s with, 1 s to settle
    rig.run(WARMUP.as_secs_f64() + 7.0);
    assert_eq!(
        rig.dev.byte(0x1151) & 0x04,
        0,
        "noise reduction is off in the second half"
    );
    test(&mut rig, TestRequest::Cancel);
    assert_eq!(rig.dev.byte(0x1151) & 0x04, 0x04);
    assert!(!mic_open(&rig));
    assert_eq!(
        outcomes(&rig),
        [TestOutcome::Cancelled(TestKind::NoiseReductionAb)]
    );
    rig.send(Command::Refresh);
    rig.run(1.0);
    assert_eq!(rig.snapshot().test, TestState::default());
    assert!(rig.snapshot().in_sync(), "{:?}", rig.snapshot());
}

#[test]
fn a_comparison_that_breaks_off_puts_the_setting_back() {
    // the microphone stops during the second half
    let mut rig = rig_in(Mode::Quiet);
    test(&mut rig, TestRequest::NoiseReductionAb { seconds: 3 });
    rig.run(7.5);
    assert_eq!(rig.dev.byte(0x1151) & 0x04, 0);
    rig.mic.lock().unwrap().fail = true;
    rig.run(0.2);
    let [(Some(TestKind::NoiseReductionAb), failure)] = &failures(&rig)[..] else {
        panic!("{:?}", failures(&rig));
    };
    assert_eq!(failure.kind, ErrorKind::Audio);
    assert_eq!(rig.dev.byte(0x1151) & 0x04, 0x04);
    assert!(!mic_open(&rig));
    assert_eq!(rig.snapshot().test, TestState::default());
}

#[test]
fn a_codec_that_does_not_take_the_switch_ends_the_comparison_and_puts_the_setting_back() {
    let mut rig = rig_in(Mode::Quiet);
    rig.dev.never_consume_newc();
    test(&mut rig, TestRequest::NoiseReductionAb { seconds: 1 });
    // noise reduction is on already: the first half needs no switch, and the switch to the
    // second half is a reload request that the ROM never takes
    until_over(&mut rig, 20.0);
    let [(Some(TestKind::NoiseReductionAb), failure)] = &failures(&rig)[..] else {
        panic!("{:?}", failures(&rig));
    };
    assert_ne!(failure.kind, ErrorKind::Invalid);
    assert!(outcomes(&rig).is_empty());
    // the bit is back as the user had it; that the reload request for it is not taken either
    // is said
    assert_eq!(rig.dev.byte(0x1151) & 0x04, 0x04);
    assert!(
        rig.warnings()
            .iter()
            .any(|w| w.contains("could not put noise reduction back"))
    );
    assert!(!mic_open(&rig));
    assert_eq!(rig.snapshot().test, TestState::default());
}

#[test]
fn only_one_test_runs_at_a_time() {
    let mut rig = rig_in(Mode::Quiet);
    test(&mut rig, TestRequest::NoiseFloor { seconds: 2 });
    rig.run(0.5);
    for request in [
        TestRequest::NoiseFloor { seconds: 1 },
        TestRequest::Record { seconds: 1 },
        TestRequest::NoiseReductionAb { seconds: 1 },
        TestRequest::PlayRecording,
        TestRequest::ClickTest,
        TestRequest::NoiseListen,
    ] {
        test(&mut rig, request);
    }
    rig.run(0.1);
    let refused = failures(&rig);
    assert_eq!(refused.len(), 6);
    assert!(refused.iter().all(|(kind, f)| kind.is_none()
        && f.kind == ErrorKind::Invalid
        && f.message.contains("another test is running")));

    // the first one is not disturbed
    until_over(&mut rig, 10.0);
    let [TestOutcome::NoiseFloor { dbfs }] = outcomes(&rig)[..] else {
        panic!("{:?}", outcomes(&rig));
    };
    near(dbfs, -60.0);
}

#[test]
fn durations_outside_their_range_are_refused_before_anything_is_opened() {
    let mut rig = rig_in(Mode::Quiet);
    let opened = rig.dev.requests();
    let bad = [
        (TestKind::NoiseFloor, TestRequest::NoiseFloor { seconds: 0 }),
        (
            TestKind::NoiseFloor,
            TestRequest::NoiseFloor { seconds: 61 },
        ),
        (TestKind::Record, TestRequest::Record { seconds: 0 }),
        (TestKind::Record, TestRequest::Record { seconds: 31 }),
        (
            TestKind::NoiseReductionAb,
            TestRequest::NoiseReductionAb { seconds: 0 },
        ),
        (
            TestKind::NoiseReductionAb,
            TestRequest::NoiseReductionAb { seconds: 31 },
        ),
    ];
    for (_, request) in bad {
        test(&mut rig, request);
    }
    rig.run(0.5);
    let got = failures(&rig);
    assert_eq!(got.len(), bad.len());
    for ((kind, _), (got_kind, failure)) in bad.iter().zip(&got) {
        assert_eq!(got_kind, &Some(*kind));
        assert_eq!(failure.kind, ErrorKind::Invalid);
    }
    assert!(!mic_open(&rig));
    assert_eq!(rig.dev.requests(), opened, "the dongle was not even read");
    assert_eq!(rig.snapshot().test, TestState::default());

    // the longest ones are accepted
    for request in [
        TestRequest::NoiseFloor { seconds: 60 },
        TestRequest::Record { seconds: 30 },
    ] {
        test(&mut rig, request);
        rig.run(0.2);
        assert!(rig.snapshot().test.running.is_some());
        test(&mut rig, TestRequest::Cancel);
    }
}

#[test]
fn cancelling_ends_the_test_and_closes_the_microphone_unless_the_meter_is_on() {
    let mut rig = rig_in(Mode::Quiet);
    test(&mut rig, TestRequest::NoiseFloor { seconds: 30 });
    rig.run(4.0);
    test(&mut rig, TestRequest::Cancel);
    assert!(!mic_open(&rig));
    assert_eq!(
        outcomes(&rig),
        [TestOutcome::Cancelled(TestKind::NoiseFloor)]
    );

    // with the meter on the microphone stays open
    test(&mut rig, TestRequest::Meter(true));
    test(&mut rig, TestRequest::Record { seconds: 30 });
    rig.run(1.0);
    test(&mut rig, TestRequest::Cancel);
    assert!(mic_open(&rig));
    assert!(rig.snapshot().test.meter);
    test(&mut rig, TestRequest::Meter(false));
    assert!(!mic_open(&rig));

    // nothing running: nothing to report
    let events = rig.test_events().len();
    test(&mut rig, TestRequest::Cancel);
    rig.run(0.2);
    assert_eq!(rig.test_events().len(), events);
}

#[test]
fn a_measurement_started_with_the_meter_on_does_not_wait_for_another_warm_up() {
    let mut rig = rig_in(Mode::Quiet);
    test(&mut rig, TestRequest::Meter(true));
    rig.run(5.0);
    let before = levels(&rig).len();
    test(&mut rig, TestRequest::NoiseFloor { seconds: 1 });
    let waited = until_over(&mut rig, 10.0);
    assert!((waited - 1.0).abs() < 0.15, "{waited}");
    // the meter went on reporting during the measurement, and is still on
    assert!(levels(&rig).len() - before >= 19);
    assert!(mic_open(&rig));
    assert!(rig.snapshot().test.meter);
}

#[test]
fn a_test_holds_the_dongle_so_the_saved_settings_wait_for_it() {
    let mut rig = rig_in(Mode::Quiet);
    let boost = |rig: &Rig| {
        rig.dev
            .writes()
            .iter()
            .filter(|(a, _)| *a == 0x1017)
            .count()
    };
    test(&mut rig, TestRequest::NoiseFloor { seconds: 2 });
    rig.run(0.5);
    rig.send(Command::SetMicBoost(5));
    rig.run(3.0);
    assert_eq!(boost(&rig), 0, "written during the measurement");
    assert!(!rig.snapshot().in_sync());
    until_over(&mut rig, 10.0);
    rig.run(1.0);
    assert_eq!(boost(&rig), 1);
    assert!(rig.snapshot().in_sync());
    assert_eq!(rig.dev.byte(0x1017), 0x3D);
}

#[test]
fn the_meter_alone_does_not_hold_the_dongle() {
    let mut rig = rig_in(Mode::Quiet);
    test(&mut rig, TestRequest::Meter(true));
    rig.run(1.0);
    rig.send(Command::SetMicBoost(5));
    rig.run(1.0);
    assert_eq!(rig.dev.byte(0x1017), 0x3D);
    assert!(rig.snapshot().in_sync());
    assert!(mic_open(&rig));
}

#[test]
fn balanced_mode_is_watched_until_the_output_is_idle_after_the_microphone_closes() {
    let mut rig = rig_in(Mode::Balanced);
    rig.dev.hold_driver_after_capture(60);
    test(&mut rig, TestRequest::Meter(true));
    rig.run(1.0);
    assert_eq!(
        rig.dev.byte(0x0F3E) & 0x02,
        0x02,
        "the references are up while the codec captures"
    );
    test(&mut rig, TestRequest::Meter(false));
    assert_eq!(rig.dev.byte(0x0F3E) & 0x02, 0, "they drop at once");
    assert_eq!(
        rig.dev.byte(0x0F1A),
        0x33,
        "the driver stays open for a while"
    );
    rig.run(3.0);
    assert_eq!(rig.dev.byte(0x0F1A), 0x77);
    let line = rig
        .log()
        .into_iter()
        .find(|l| l.contains("the output is idle again"))
        .expect("the return to idle is logged");
    let waited: u32 = line
        .split("idle again ")
        .nth(1)
        .and_then(|rest| rest.split(' ').next())
        .and_then(|n| n.parse().ok())
        .unwrap();
    assert!((800..2000).contains(&waited), "{line}");

    // it stopped looking: no more reads of the dongle on its own
    let requests = rig.dev.requests();
    rig.run(2.0);
    assert_eq!(rig.dev.requests(), requests);
    assert!(rig.warnings().is_empty());
}

#[test]
fn an_output_that_does_not_return_to_idle_is_reported_after_a_while() {
    let mut rig = rig_in(Mode::Balanced);
    rig.dev.hold_driver_after_capture(usize::MAX);
    test(&mut rig, TestRequest::Meter(true));
    rig.run(0.5);
    test(&mut rig, TestRequest::Meter(false));
    rig.run(14.0);
    assert!(rig.warnings().is_empty());
    rig.run(2.0);
    assert!(
        rig.warnings()
            .iter()
            .any(|w| w.contains("did not return to idle within 15 s"))
    );
    let requests = rig.dev.requests();
    rig.run(2.0);
    assert_eq!(rig.dev.requests(), requests, "watching ended");
}

#[test]
fn modes_that_do_not_change_while_capturing_are_not_watched() {
    for mode in [Mode::Stock, Mode::Quiet, Mode::MinHiss] {
        let mut rig = rig_in(mode);
        test(&mut rig, TestRequest::Meter(true));
        rig.run(1.0);
        test(&mut rig, TestRequest::Meter(false));
        let requests = rig.dev.requests();
        rig.run(3.0);
        assert_eq!(rig.dev.requests(), requests, "{mode:?}");
        assert!(
            !rig.log().iter().any(|l| l.contains("idle again")),
            "{mode:?}"
        );
    }
}

#[test]
fn a_microphone_that_cannot_be_opened_is_a_failed_request_and_changes_nothing() {
    let mut rig = rig_in(Mode::Quiet);
    rig.capture_fails.store(true, Ordering::SeqCst);
    test(&mut rig, TestRequest::Meter(true));
    test(&mut rig, TestRequest::NoiseFloor { seconds: 1 });
    test(&mut rig, TestRequest::NoiseReductionAb { seconds: 1 });
    rig.run(0.5);
    let got = failures(&rig);
    assert_eq!(got.len(), 3);
    assert_eq!(got[0].0, None);
    assert_eq!(got[1].0, Some(TestKind::NoiseFloor));
    assert_eq!(got[2].0, Some(TestKind::NoiseReductionAb));
    assert!(got.iter().all(|(_, f)| f.kind == ErrorKind::Audio));
    assert_eq!(rig.snapshot().test, TestState::default());
    // noise reduction was not touched
    assert_eq!(rig.dev.byte(0x1151), 0x04);
    assert!(rig.errors().is_empty(), "the settings have no error");
}

#[test]
fn a_microphone_that_stops_by_itself_ends_the_test_with_an_error() {
    let mut rig = rig_in(Mode::Quiet);
    test(&mut rig, TestRequest::Meter(true));
    test(&mut rig, TestRequest::Record { seconds: 5 });
    rig.run(4.0);
    rig.mic.lock().unwrap().fail = true;
    rig.run(0.2);
    let [(Some(TestKind::Record), failure)] = &failures(&rig)[..] else {
        panic!("{:?}", failures(&rig));
    };
    assert_eq!(failure.kind, ErrorKind::Audio);
    assert!(
        !mic_open(&rig),
        "the failed stream is closed, the meter with it"
    );
    assert_eq!(rig.snapshot().test, TestState::default());
    assert!(outcomes(&rig).is_empty());
}

#[test]
fn a_microphone_failure_lets_go_of_the_dongle_in_every_mode() {
    for mode in Mode::ALL {
        let mut rig = rig_in(mode);
        let before = rig.dev.handles();
        test(&mut rig, TestRequest::NoiseReductionAb { seconds: 3 });
        rig.run(WARMUP.as_secs_f64() + 0.5);
        assert!(
            rig.dev.handles() > before,
            "{mode:?}: the test reads the dongle"
        );
        rig.mic.lock().unwrap().fail = true;
        rig.run(0.2);
        assert_eq!(failures(&rig).len(), 1, "{mode:?}");
        // long enough for the balanced mode to settle, short of the next pass over the dongle
        rig.run(3.0);
        assert_eq!(rig.dev.handles(), before, "{mode:?}: a handle stays open");
        assert_eq!(rig.snapshot().test, TestState::default());
    }
}

#[test]
fn a_microphone_that_delivers_nothing_ends_the_test_after_three_seconds() {
    let mut rig = rig_in(Mode::Quiet);
    rig.mic.lock().unwrap().stall = true;
    test(&mut rig, TestRequest::NoiseFloor { seconds: 2 });
    // nothing arrives from the start, which is noticed before the warm-up is over
    let waited = until_over(&mut rig, 20.0);
    assert!((waited - 3.0).abs() < 0.2, "{waited}");
    let [(Some(TestKind::NoiseFloor), failure)] = &failures(&rig)[..] else {
        panic!("{:?}", failures(&rig));
    };
    assert_eq!(failure.kind, ErrorKind::Audio);
    assert!(
        failure.message.contains("delivers no data"),
        "{}",
        failure.message
    );
    assert!(!mic_open(&rig));
}

#[test]
fn a_clip_that_stops_by_itself_ends_the_test_with_an_error() {
    let mut rig = rig_in(Mode::Quiet);
    test(&mut rig, TestRequest::ClickTest);
    rig.run(1.0);
    assert!(playing(&rig));
    rig.playback_fails.store(true, Ordering::SeqCst);
    rig.run(0.2);
    let [(Some(TestKind::ClickTest), failure)] = &failures(&rig)[..] else {
        panic!("{:?}", failures(&rig));
    };
    assert_eq!(failure.kind, ErrorKind::Audio);
    assert!(!playing(&rig));
    assert!(outcomes(&rig).is_empty());
}

#[test]
fn a_clip_that_cannot_be_started_ends_the_test_with_an_error() {
    let mut rig = rig_in(Mode::Quiet);
    rig.dev.remove_endpoint();
    test(&mut rig, TestRequest::ClickTest);
    until_over(&mut rig, 5.0);
    let [(Some(TestKind::ClickTest), failure)] = &failures(&rig)[..] else {
        panic!("{:?}", failures(&rig));
    };
    assert_eq!(failure.kind, ErrorKind::Audio);
    assert_eq!(rig.snapshot().test, TestState::default());
}

#[test]
fn a_cancelled_clip_stops_playing() {
    let mut rig = rig_in(Mode::Balanced);
    test(&mut rig, TestRequest::ClickTest);
    rig.run(1.0);
    assert!(playing(&rig));
    test(&mut rig, TestRequest::Cancel);
    assert!(!playing(&rig));
    assert_eq!(
        outcomes(&rig),
        [TestOutcome::Cancelled(TestKind::ClickTest)]
    );
}

#[test]
fn requests_with_no_dongle_are_refused() {
    let mut rig = Rig::new(Config::default(), false);
    rig.run(1.0);
    for request in [
        TestRequest::Meter(true),
        TestRequest::NoiseFloor { seconds: 1 },
        TestRequest::ClickTest,
    ] {
        test(&mut rig, request);
    }
    let got = failures(&rig);
    assert_eq!(got.len(), 3);
    assert!(
        got.iter()
            .all(|(kind, f)| kind.is_none() && f.kind == ErrorKind::Communication)
    );
    assert!(got[0].1.message.contains("not connected"));
    assert!(!mic_open(&rig));
    // closing the meter or cancelling needs no dongle
    test(&mut rig, TestRequest::Meter(false));
    test(&mut rig, TestRequest::Cancel);
    assert_eq!(failures(&rig).len(), 3);
}

#[test]
fn a_dongle_that_disconnects_ends_the_test_and_everything_it_opened() {
    let mut rig = rig_in(Mode::Quiet);
    test(&mut rig, TestRequest::NoiseReductionAb { seconds: 3 });
    rig.run(7.5);
    rig.dev.unplug();
    rig.set_present(false);
    rig.run(2.0);
    let [(Some(TestKind::NoiseReductionAb), failure)] = &failures(&rig)[..] else {
        panic!("{:?}", failures(&rig));
    };
    assert_eq!(failure.kind, ErrorKind::Communication);
    assert!(
        failure.message.contains("disconnected"),
        "{}",
        failure.message
    );
    assert_eq!(rig.snapshot().test, TestState::default());
    // the setting could not be put back on a dongle that is gone, and that is said
    assert!(
        rig.warnings()
            .iter()
            .any(|w| w.contains("could not put noise reduction back"))
    );
    // the engine puts it right when the dongle is there again and is read
    rig.dev.plug_in();
    rig.set_present(true);
    rig.run(40.0);
    assert!(rig.snapshot().in_sync(), "{:?}", rig.snapshot());
    assert_eq!(rig.dev.byte(0x1151) & 0x04, 0x04);
}

#[test]
fn waking_from_sleep_and_stopping_the_engine_end_the_test() {
    let mut rig = rig_in(Mode::Quiet);
    test(&mut rig, TestRequest::NoiseReductionAb { seconds: 3 });
    rig.run(7.5);
    rig.sup.resumed();
    assert!(!mic_open(&rig));
    let [(Some(TestKind::NoiseReductionAb), failure)] = &failures(&rig)[..] else {
        panic!("{:?}", failures(&rig));
    };
    assert!(failure.message.contains("went to sleep"));

    test(&mut rig, TestRequest::Meter(true));
    rig.run(0.5);
    rig.sup.finish();
    assert!(!mic_open(&rig));
    assert_eq!(failures(&rig).len(), 2);
    assert_eq!(failures(&rig)[1].0, None);
    assert!(failures(&rig)[1].1.message.contains("stopped"));
}

/// The noise reduction bit of the dongle, and the number of writes to `$1151` so far.
fn nr_state(rig: &Rig) -> (bool, usize) {
    let writes = rig
        .dev
        .writes()
        .iter()
        .filter(|(a, _)| *a == 0x1151)
        .count();
    (rig.dev.byte(0x1151) & 0x04 != 0, writes)
}

#[test]
fn after_a_sleep_noise_reduction_is_put_back_once_the_dongle_has_recovered() {
    // chosen (the settings pass would correct it) and not chosen (only the put-back can)
    for chosen in [Some(true), None] {
        let cfg = Config {
            mode: Some(Mode::Quiet),
            mic_nr: chosen,
            ..Config::default()
        };
        let mut rig = Rig::with_device(FakeDevice::in_mode(Mode::Quiet), cfg, true);
        rig.run(5.0);
        assert!(rig.snapshot().in_sync());
        test(&mut rig, TestRequest::NoiseReductionAb { seconds: 3 });
        // in the second half, with noise reduction off
        rig.run(WARMUP.as_secs_f64() + 3.0 + 1.5);
        let (on, writes) = nr_state(&rig);
        assert!(!on, "{chosen:?}: the comparison is in its second half");

        rig.sup.resumed();
        assert!(!mic_open(&rig));
        assert_eq!(
            nr_state(&rig),
            (false, writes),
            "{chosen:?}: no write at the wake"
        );
        rig.run(SETTLE.as_secs_f64() - 0.5);
        assert_eq!(
            nr_state(&rig),
            (false, writes),
            "{chosen:?}: the dongle is still left alone"
        );
        rig.run(2.0);
        assert_eq!(nr_state(&rig), (true, writes + 1), "{chosen:?}: put back");
        assert!(rig.snapshot().in_sync(), "{chosen:?}");
        assert!(
            rig.warnings().is_empty(),
            "{chosen:?}: {:?}",
            rig.warnings()
        );
        assert!(rig.errors().is_empty(), "{chosen:?}");
    }
}

#[test]
fn a_second_sleep_before_the_put_back_still_puts_the_original_setting_back() {
    let cfg = Config {
        mode: Some(Mode::Quiet),
        ..Config::default()
    };
    let mut rig = Rig::with_device(FakeDevice::in_mode(Mode::Quiet), cfg, true);
    rig.run(5.0);
    assert!(nr_state(&rig).0);
    let second_half = WARMUP.as_secs_f64() + 1.0 + 1.5;
    test(&mut rig, TestRequest::NoiseReductionAb { seconds: 1 });
    rig.run(second_half);
    assert!(!nr_state(&rig).0);
    rig.sup.resumed();
    // a comparison that starts before the put-back sees noise reduction off, which is not the
    // user's setting
    test(&mut rig, TestRequest::NoiseReductionAb { seconds: 1 });
    rig.run(second_half);
    rig.sup.resumed();
    rig.run(SETTLE.as_secs_f64() + 1.0);
    assert!(nr_state(&rig).0, "the user's setting is back");
    assert!(rig.warnings().is_empty(), "{:?}", rig.warnings());
}

#[test]
fn the_state_says_what_is_running_and_whether_there_is_a_recording() {
    let mut rig = rig_in(Mode::Quiet);
    let state = |rig: &Rig| rig.snapshot().test;
    assert_eq!(state(&rig), TestState::default());
    test(&mut rig, TestRequest::Meter(true));
    rig.run(0.2);
    assert_eq!(
        state(&rig),
        TestState {
            meter: true,
            running: None,
            has_recording: false
        }
    );
    test(&mut rig, TestRequest::Record { seconds: 1 });
    rig.run(0.2);
    assert_eq!(
        state(&rig),
        TestState {
            meter: true,
            running: Some(TestKind::Record),
            has_recording: false
        }
    );
    until_over(&mut rig, 10.0);
    rig.run(0.1);
    assert_eq!(
        state(&rig),
        TestState {
            meter: true,
            running: None,
            has_recording: true
        }
    );
    // other tests do not lose it
    test(&mut rig, TestRequest::Meter(false));
    test(&mut rig, TestRequest::NoiseListen);
    until_over(&mut rig, 10.0);
    rig.run(0.1);
    assert_eq!(
        state(&rig),
        TestState {
            meter: false,
            running: None,
            has_recording: true
        }
    );
}
