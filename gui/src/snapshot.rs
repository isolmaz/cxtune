//! Development only (`--features snapshot`): `--snapshot <folder>` renders the window to PNG files
//! in both languages, then quits. The files show every page in both colour schemes, the other
//! modes of the output page and the status of every problem. Every state is simulated: the engine
//! is not started, so the dongle is never read or written, and the header meters show made-up
//! levels in one picture.

use std::path::{Path, PathBuf};
use std::time::Duration;

use cxtune::config::Config;
use cxtune::engine::{Phase, Readback, Snapshot};
use cxtune::mode::Mode;
use slint::language::ColorScheme;
use slint::{ComponentHandle, Timer, TimerMode};

use crate::app::{self, App};
use crate::lang::Lang;
use crate::status::{self, Problem};
use crate::ui::{Hint, MainWindow, Model, Theme, Title, Tone};

/// Gives the window time to draw after each change.
const STEP: Duration = Duration::from_millis(600);
const TABS: [&str; 5] = ["output", "microphone", "sidetone", "general", "about"];
const MODES: [&str; 4] = ["default", "click-free", "balanced", "lowest-noise"];
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
            mic_nr: Some(true),
            mic_agc: Some(false),
            sidetone: Some(false),
            sidetone_level: None,
        },
        settings_saved: true,
        save_error: None,
        device: Some(Readback {
            firmware_supported: true,
            mode: Some(mode),
            min_hiss_ms,
            mic_boost: Some(6),
            noise_reduction: true,
            agc: false,
            sidetone: false,
            sidetone_level: 947,
        }),
        error: None,
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
        } else {
            save(&window, &folder.join(shot.name()));
        }
        tick += 1;
    });
    std::mem::forget(timer);
}

fn save(window: &MainWindow, path: &Path) {
    let buffer = window.window().take_snapshot().expect("a snapshot");
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
