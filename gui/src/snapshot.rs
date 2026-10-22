//! Development only (`--features snapshot`): `--snapshot <folder>` renders the window to PNG files
//! in both languages, then quits. The files show every page in both colour schemes, the other
//! modes of the output page and the status of every problem. Every state is simulated: the engine
//! is not started, so the dongle is never read or written, and the header meters show made-up
//! levels in one picture. The folder `grid` inside the folder holds the same pictures with a
//! guide line at each x that the layout promises (docs/gui.md), to check them by eye.

use std::path::{Path, PathBuf};
use std::time::Duration;

use cxtune::config::Config;
use cxtune::engine::{Phase, Readback, Snapshot, TestState};
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
const FOCUS: [(usize, usize); 5] = [(0, 7), (1, 10), (2, 6), (3, 7), (4, 3)];
const TABS: [&str; 5] = ["output", "microphone", "sidetone", "general", "about"];
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
    /// With levels on the header meters.
    Meters,
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
            Scene::Meters => "output-meters".into(),
            Scene::Problem(_, name) => format!("error-{name}"),
            Scene::SaveFailed => "error-save-failed".into(),
            Scene::Warned => "warning".into(),
            Scene::Disconnected => "disconnected".into(),
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

fn shots() -> Vec<Shot> {
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
        shot(ColorScheme::Dark, 0, Scene::Meters);
        shot(ColorScheme::Light, 0, Scene::Meters);
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
        shot(ColorScheme::Light, 3, Scene::AutostartFailed);
        for (tab, presses) in FOCUS {
            if lang == Lang::English {
                for press in 1..=presses {
                    shot(ColorScheme::Light, tab, Scene::Focus(press));
                    shot(ColorScheme::Dark, tab, Scene::Focus(press));
                }
            }
        }
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
fn simulate(window: &MainWindow, scene: Scene) {
    let model = window.global::<Model>();
    let mode = match scene {
        Scene::Mode(mode) => Mode::ALL[mode],
        _ => Mode::Balanced,
    };
    let state = match scene {
        Scene::SidetoneOn => sidetone_on(connected(mode)),
        _ => connected(mode),
    };
    app::with_app(|app| app.preview(state));

    let present = !matches!(scene, Scene::Disconnected);
    model.set_controls_enabled(present);
    model.set_delay_enabled(present && mode == Mode::MinHiss);
    model.set_speaker_present(present);
    model.set_microphone_present(present);
    model.set_speaker_volume(45.0);
    model.set_microphone_volume(80.0);
    let muted = matches!(scene, Scene::Muted);
    model.set_speaker_muted(muted);
    model.set_microphone_muted(muted);
    model.set_autostart(true);
    model.set_autostart_failed(matches!(scene, Scene::AutostartFailed));
    let meters = matches!(scene, Scene::Meters);
    model.set_meters_active(meters);
    model.set_out_db(-14.0);
    model.set_mic_db(-27.0);

    match scene {
        Scene::Problem(problem, _) => {
            let shown = status::status(None, Some(problem), false);
            model.set_title(shown.title);
            model.set_hint(shown.hint);
            model.set_tone(shown.tone);
            model.set_details(problem != Problem::UnsupportedFirmware);
            model.set_reapply_enabled(shown.reapply);
            model.set_controls_enabled(shown.controls);
            model.set_delay_enabled(false);
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

pub fn schedule(window: &MainWindow, folder: PathBuf) {
    let shots = shots();
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
            simulate(&window, shot.scene);
            if matches!(shot.scene, Scene::Focus(_)) && shot.scheme == ColorScheme::Light {
                press_tab(&window);
            }
        } else {
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
