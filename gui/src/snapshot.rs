//! Development only (`--features snapshot`): `--snapshot <folder>` renders the window to PNG files
//! in both languages, then quits. The files show every page in both colour schemes, the other
//! modes of the output page and the status of every problem; `--pages output,test` renders only
//! those pages, without the problems. Every state is simulated: the engine is not started, so the
//! dongle is never read or written, and the meter and the results of the Test page are made up.
//! The folder `grid` inside the folder holds the same pictures with a guide line at each x that
//! the layout promises (docs/gui.md), to check them by eye.

use std::path::{Path, PathBuf};
use std::time::Duration;

use cxtune::capture::Level;
use cxtune::config::Config;
use cxtune::engine::{
    ErrorKind, Failure, Phase, Readback, Snapshot, TestEvent, TestKind, TestOutcome, TestProgress,
    TestState, TestStep,
};
use cxtune::mode::Mode;
use slint::language::ColorScheme;
use slint::platform::{Key, WindowEvent};
use slint::{ComponentHandle, Rgba8Pixel, SharedPixelBuffer, Timer, TimerMode};

use crate::app::{self, App};
use crate::lang::Lang;
use crate::status::{self, Problem};
use crate::ui::{Hint, MainWindow, Model, Theme, Title, Tone};

/// Gives the window time to draw after each change.
const STEP: Duration = Duration::from_millis(600);

/// The x of the guide lines, in logical pixels of a window of `WIDTH`, and their colours: the
/// left edge of the page, where every label starts, where every slider starts and ends, where
/// every value and every control ends, and the right edge of the page. They follow from the
/// spec (navigation 160, page padding 24, card padding 12, glyph column 20, gap 12, slider 200,
/// value 64 after a gap of 8), not from the interface files.
const WIDTH: u32 = 720;
const GUIDES: [(u32, [u8; 3]); 6] = [
    (184, [0, 90, 255]),
    (228, [0, 170, 60]),
    (412, [255, 0, 0]),
    (612, [255, 0, 0]),
    (684, [255, 0, 200]),
    (696, [0, 90, 255]),
];
/// The pages and the number of controls that Tab reaches on them (English only, in `focus`).
const FOCUS: [(usize, usize); 6] = [(0, 12), (1, 10), (2, 6), (3, 13), (4, 7), (5, 3)];
const TABS: [&str; 6] = [
    "output",
    "microphone",
    "sidetone",
    "test",
    "general",
    "about",
];
const TEST_TAB: usize = 3;
const MODES: [&str; 4] = ["default", "low-click", "balanced", "lowest-noise"];
const PROBLEMS: [(Problem, &str); 12] = [
    (Problem::Communication, "communication"),
    (Problem::Audio, "audio"),
    (Problem::Refused, "refused"),
    (Problem::UnsupportedFirmware, "unsupported-firmware"),
    (Problem::Verify, "verify"),
    (Problem::Drift, "drift"),
    (Problem::Unresponsive, "unresponsive"),
    (Problem::Fatal, "fatal"),
    (Problem::Busy, "busy"),
    (Problem::BadSettingsFile, "bad-settings-file"),
    (Problem::StartFailed, "start-failed"),
    (Problem::Other, "other"),
];

#[derive(Clone, Copy)]
enum Scene {
    /// A dongle in sync in balanced mode.
    Connected,
    /// The same with another mode chosen.
    Mode(usize),
    /// Sidetone on at a low level.
    SidetoneOn,
    /// Both volumes muted.
    Muted,
    /// Virtual surround on, at a level.
    SurroundOn,
    /// The Test page with the results of three tests.
    TestResults,
    /// The Test page while the noise reduction comparison runs.
    TestRunning,
    /// The Test page after a request that failed.
    TestProblem,
    Problem(Problem, &'static str),
    SaveFailed,
    Warned,
    Disconnected,
    AutostartFailed,
    /// The keyboard focus after one more press of Tab: the pictures of a page follow each other.
    Focus(usize),
}

struct Shot {
    lang: Lang,
    scheme: ColorScheme,
    tab: usize,
    scene: Scene,
}

impl Shot {
    fn name(&self) -> String {
        let scheme = if self.scheme == ColorScheme::Dark {
            "dark"
        } else {
            "light"
        };
        let what = match self.scene {
            Scene::Connected => TABS[self.tab].to_string(),
            Scene::Mode(mode) => format!("output-{}", MODES[mode]),
            Scene::SidetoneOn => "sidetone-on".into(),
            Scene::Muted => "output-muted".into(),
            Scene::SurroundOn => "output-surround".into(),
            Scene::TestResults => "test-results".into(),
            Scene::TestRunning => "test-running".into(),
            Scene::TestProblem => "test-problem".into(),
            Scene::Problem(_, name) => format!("error-{name}"),
            Scene::SaveFailed => "error-save-failed".into(),
            Scene::Warned => "warning".into(),
            Scene::Disconnected if self.tab == 0 => "disconnected".into(),
            Scene::Disconnected => format!("{}-disconnected", TABS[self.tab]),
            Scene::AutostartFailed => "general-autostart-failed".into(),
            Scene::Focus(press) => {
                return format!(
                    "focus/{}-{scheme}-{}-focus-{press:02}.png",
                    self.lang.code(),
                    TABS[self.tab]
                );
            }
        };
        format!("{}-{scheme}-{what}.png", self.lang.code())
    }
}

/// The pictures of `pages` only (their names as in `TABS`): each page, the pages' other states,
/// and none of the problems of the status.
fn wanted(shot: &Shot, pages: &[String]) -> bool {
    pages.iter().any(|page| page == TABS[shot.tab])
        && matches!(
            shot.scene,
            Scene::Connected
                | Scene::Mode(3)
                | Scene::SurroundOn
                | Scene::TestResults
                | Scene::TestRunning
                | Scene::TestProblem
                | Scene::Disconnected
        )
}

fn shots(pages: Option<&[String]>) -> Vec<Shot> {
    let mut shots = Vec::new();
    for lang in [Lang::Turkish, Lang::English] {
        let mut shot = |scheme, tab, scene| {
            shots.push(Shot {
                lang,
                scheme,
                tab,
                scene,
            });
        };
        for tab in 0..TABS.len() {
            shot(ColorScheme::Light, tab, Scene::Connected);
            shot(ColorScheme::Dark, tab, Scene::Connected);
        }
        for mode in [0, 1, 3] {
            shot(ColorScheme::Light, 0, Scene::Mode(mode));
            shot(ColorScheme::Dark, 0, Scene::Mode(mode));
        }
        shot(ColorScheme::Dark, 2, Scene::SidetoneOn);
        shot(ColorScheme::Dark, 0, Scene::Muted);
        shot(ColorScheme::Light, 0, Scene::SurroundOn);
        shot(ColorScheme::Dark, 0, Scene::SurroundOn);
        for scheme in [ColorScheme::Light, ColorScheme::Dark] {
            shot(scheme, TEST_TAB, Scene::TestResults);
            shot(scheme, TEST_TAB, Scene::TestRunning);
        }
        shot(ColorScheme::Light, TEST_TAB, Scene::TestProblem);
        shot(ColorScheme::Dark, TEST_TAB, Scene::Disconnected);
        for (problem, name) in PROBLEMS {
            shot(ColorScheme::Light, 0, Scene::Problem(problem, name));
        }
        shot(
            ColorScheme::Dark,
            0,
            Scene::Problem(Problem::Communication, "communication"),
        );
        shot(ColorScheme::Light, 0, Scene::SaveFailed);
        shot(ColorScheme::Light, 0, Scene::Warned);
        shot(ColorScheme::Light, 0, Scene::Disconnected);
        shot(ColorScheme::Dark, 0, Scene::Disconnected);
        shot(ColorScheme::Light, 4, Scene::AutostartFailed);
        for (tab, presses) in FOCUS {
            if lang == Lang::English {
                for press in 1..=presses {
                    shot(ColorScheme::Light, tab, Scene::Focus(press));
                    shot(ColorScheme::Dark, tab, Scene::Focus(press));
                }
            }
        }
    }
    if let Some(pages) = pages {
        shots.retain(|shot| wanted(shot, pages));
    }
    shots
}

/// A dongle that holds exactly what the settings say.
fn connected(mode: Mode) -> Snapshot {
    let min_hiss_ms = (mode == Mode::MinHiss).then_some(600);
    Snapshot {
        phase: Phase::Connected,
        settings: Config {
            mode: Some(mode),
            mic_boost: Some(6),
            min_hiss_ms,
            balanced_decay_ms: None,
            mic_nr: Some(true),
            mic_agc: Some(false),
            sidetone: Some(false),
            sidetone_level: None,
            surround: None,
            surround_level: None,
        },
        settings_saved: true,
        save_error: None,
        device: Some(Readback {
            firmware_supported: true,
            mode: Some(mode),
            min_hiss_ms,
            balanced_decay_ms: None,
            mic_boost: Some(6),
            noise_reduction: true,
            agc: false,
            sidetone: false,
            sidetone_level: 947,
            surround: false,
            surround_level: None,
        }),
        error: None,
        test: TestState::default(),
    }
}

fn surround_on(mut snapshot: Snapshot) -> Snapshot {
    snapshot.settings.surround = Some(true);
    snapshot.settings.surround_level = Some(7);
    if let Some(device) = &mut snapshot.device {
        device.surround = true;
        device.surround_level = Some(7);
    }
    snapshot
}

/// The messages that fill the Test page in `scene`: the meter reads -52 dBFS wherever it runs.
fn test_events(scene: Scene) -> Vec<TestEvent> {
    let level = TestEvent::Level(Level {
        rms_dbfs: -52.0,
        peak_dbfs: -40.0,
    });
    let results = [
        TestEvent::Done(TestOutcome::NoiseFloor { dbfs: -63.9 }),
        TestEvent::Done(TestOutcome::Recorded {
            seconds: 5.0,
            rms_dbfs: -41.2,
            peak_dbfs: -18.0,
        }),
    ];
    match scene {
        Scene::TestResults => {
            let comparison = TestEvent::Done(TestOutcome::NoiseReductionAb {
                with_dbfs: -63.9,
                without_dbfs: -52.0,
                reduction_db: 11.9,
            });
            let [floor, recording] = results;
            vec![level, floor, recording, comparison]
        }
        Scene::TestRunning => {
            let [floor, _] = results;
            vec![
                level,
                floor,
                TestEvent::Done(TestOutcome::ClickTest { waited_ms: 812 }),
                TestEvent::Progress(TestProgress {
                    kind: TestKind::NoiseReductionAb,
                    step: TestStep::MeasuringWithoutNoiseReduction,
                    remaining_ms: Some(7_300),
                }),
            ]
        }
        Scene::TestProblem => vec![TestEvent::Failed {
            kind: None,
            failure: Failure {
                kind: ErrorKind::Audio,
                message: "the microphone is in use".into(),
            },
        }],
        Scene::Disconnected => Vec::new(),
        _ => vec![level],
    }
}

fn sidetone_on(mut snapshot: Snapshot) -> Snapshot {
    snapshot.settings.sidetone = Some(true);
    snapshot.settings.sidetone_level = Some(200);
    if let Some(device) = &mut snapshot.device {
        device.sidetone = true;
        device.sidetone_level = 200;
    }
    snapshot
}

/// Shows the scene: the state of the engine goes through the same code as the real one; what the
/// engine does not report (the volumes, the meters, a window that has an engine) is set here.
fn simulate(window: &MainWindow, scene: Scene, tab: usize) {
    let model = window.global::<Model>();
    let mode = match scene {
        Scene::Mode(mode) => Mode::ALL[mode],
        _ => Mode::Balanced,
    };
    let mut state = match scene {
        Scene::SidetoneOn => sidetone_on(connected(mode)),
        Scene::SurroundOn => surround_on(connected(mode)),
        _ => connected(mode),
    };
    // the meter runs on the Test page, except where there is no dongle or the last request failed
    let events = if tab == TEST_TAB {
        state.test = TestState {
            meter: !matches!(scene, Scene::Disconnected | Scene::TestProblem),
            running: matches!(scene, Scene::TestRunning).then_some(TestKind::NoiseReductionAb),
            has_recording: matches!(scene, Scene::TestResults | Scene::TestRunning),
        };
        test_events(scene)
    } else {
        Vec::new()
    };
    app::with_app(|app| app.preview(state, &events));

    let present = !matches!(scene, Scene::Disconnected);
    model.set_controls_enabled(present);
    model.set_test_available(present);
    model.set_settle_enabled(present && mode == Mode::MinHiss);
    model.set_decay_enabled(present && mode == Mode::Balanced);
    model.set_speaker_present(present);
    model.set_microphone_present(present);
    model.set_speaker_volume(45.0);
    model.set_microphone_volume(80.0);
    let muted = matches!(scene, Scene::Muted);
    model.set_speaker_muted(muted);
    model.set_microphone_muted(muted);
    model.set_autostart(true);
    model.set_autostart_failed(matches!(scene, Scene::AutostartFailed));

    match scene {
        Scene::Problem(problem, _) => {
            let shown = status::status(None, Some(problem), false);
            model.set_title(shown.title);
            model.set_hint(shown.hint);
            model.set_tone(shown.tone);
            model.set_details(problem != Problem::UnsupportedFirmware);
            model.set_reapply_enabled(shown.reapply);
            model.set_controls_enabled(shown.controls);
            model.set_settle_enabled(false);
        }
        Scene::SaveFailed => {
            model.set_title(Title::SaveFailed);
            model.set_hint(Hint::CheckFile);
            model.set_tone(Tone::Caution);
            model.set_details(true);
        }
        Scene::Warned => {
            model.set_hint(Hint::SeeLog);
            model.set_tone(Tone::Caution);
            model.set_details(true);
        }
        Scene::Disconnected => {
            model.set_title(Title::Disconnected);
            model.set_hint(Hint::None);
            model.set_tone(Tone::Caution);
            model.set_reapply_enabled(false);
        }
        _ => {}
    }
}

pub fn schedule(window: &MainWindow, folder: PathBuf, pages: Option<Vec<String>>) {
    let shots = shots(pages.as_deref());
    let window = window.as_weak();
    let _ = std::fs::create_dir_all(&folder);
    let mut tick = 0;
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, STEP, move || {
        let Some(window) = window.upgrade() else {
            return;
        };
        let Some(shot) = shots.get(tick / 2) else {
            app::with_app(App::quit);
            return;
        };
        if tick % 2 == 0 {
            app::with_app(|app| app.apply_language(shot.lang));
            window.global::<Theme>().set_scheme(shot.scheme);
            window.global::<Model>().set_tab(shot.tab as i32);
            simulate(&window, shot.scene, shot.tab);
            if matches!(shot.scene, Scene::Focus(_)) && shot.scheme == ColorScheme::Light {
                press_tab(&window);
            }
        } else {
            // the change of page has rendered the window by now, and no dongle is behind it
            simulate(&window, shot.scene, shot.tab);
            save(&window, &folder, &shot.name());
        }
        tick += 1;
    });
    std::mem::forget(timer);
}

/// Moves the keyboard focus as the user would, from the first control of the window.
fn press_tab(window: &MainWindow) {
    let window = window.window();
    window.dispatch_event(WindowEvent::WindowActiveChanged(true));
    window.dispatch_event(WindowEvent::KeyPressed {
        text: Key::Tab.into(),
    });
    window.dispatch_event(WindowEvent::KeyReleased {
        text: Key::Tab.into(),
    });
}

fn save(window: &MainWindow, folder: &Path, name: &str) {
    let buffer = window.window().take_snapshot().expect("a snapshot");
    if name.starts_with("focus/") {
        let _ = std::fs::create_dir_all(folder.join("focus"));
        write_png(&buffer, &folder.join(name));
        return;
    }
    write_png(&buffer, &folder.join(name));
    let mut guided = buffer.clone();
    draw_guides(&mut guided);
    let grid = folder.join("grid");
    let _ = std::fs::create_dir_all(&grid);
    write_png(&guided, &grid.join(name));
}

/// Lines one pixel wide, over the picture.
fn draw_guides(buffer: &mut SharedPixelBuffer<Rgba8Pixel>) {
    let (width, height) = (buffer.width(), buffer.height());
    let pixels = buffer.make_mut_slice();
    for (x, [r, g, b]) in GUIDES {
        let x = x * width / WIDTH;
        for y in 0..height {
            pixels[(y * width + x) as usize] = Rgba8Pixel::new(r, g, b, 255);
        }
    }
}

fn write_png(buffer: &SharedPixelBuffer<Rgba8Pixel>, path: &Path) {
    let file = std::fs::File::create(path).expect("a file for the snapshot");
    let mut encoder = png::Encoder::new(
        std::io::BufWriter::new(file),
        buffer.width(),
        buffer.height(),
    );
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .and_then(|mut writer| writer.write_image_data(buffer.as_bytes()))
        .expect("a PNG");
}
