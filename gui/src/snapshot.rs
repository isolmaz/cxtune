//! Development only (`--features snapshot`): `--snapshot <folder>` renders the window to PNG files
//! in both languages, then quits. The files show every tab, the other modes of the output tab and
//! the status line for every problem. The problems are simulated; the rest is the real state of
//! the dongle.

use std::path::{Path, PathBuf};
use std::time::Duration;

use slint::language::ColorScheme;
use slint::{ComponentHandle, Timer, TimerMode};

use crate::app::{self, App};
use crate::lang::Lang;
use crate::status::{self, Problem};
use crate::ui::{Hint, MainWindow, Model, Theme, Title, Tone};

/// Waits for the engine to report, so that the pictures show the state of the dongle.
const FIRST: Duration = Duration::from_secs(4);
/// Gives the window time to draw after each change.
const STEP: Duration = Duration::from_millis(600);
const TABS: [&str; 5] = ["output", "microphone", "sidetone", "general", "about"];
const MODES: [&str; 4] = ["stock", "quiet", "balanced", "minimum"];
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
    /// The state of the dongle.
    Real,
    /// The output tab with another mode chosen.
    Mode(usize),
    Problem(Problem, &'static str),
    SaveFailed,
    Warned,
    Disconnected,
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
            Scene::Real => TABS[self.tab].to_string(),
            Scene::Mode(mode) => format!("output-{}", MODES[mode]),
            Scene::Problem(_, name) => format!("error-{name}"),
            Scene::SaveFailed => "error-save-failed".into(),
            Scene::Warned => "warning".into(),
            Scene::Disconnected => "disconnected".into(),
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
            shot(ColorScheme::Light, tab, Scene::Real);
            shot(ColorScheme::Dark, tab, Scene::Real);
        }
        for mode in [0, 1, 3] {
            shot(ColorScheme::Light, 0, Scene::Mode(mode));
        }
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
    }
    shots
}

fn simulate(window: &MainWindow, scene: Scene) {
    let model = window.global::<Model>();
    match scene {
        Scene::Real => {}
        Scene::Mode(mode) => model.set_mode(mode as i32),
        Scene::Problem(problem, _) => {
            let shown = status::status(None, Some(problem), false);
            model.set_title(shown.title);
            model.set_hint(shown.hint);
            model.set_tone(shown.tone);
            model.set_details(problem != Problem::UnsupportedFirmware);
        }
        Scene::SaveFailed => {
            model.set_title(Title::SaveFailed);
            model.set_hint(Hint::CheckFile);
            model.set_tone(Tone::Caution);
            model.set_details(true);
        }
        Scene::Warned => {
            model.set_title(Title::InSync);
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
    }
}

pub fn schedule(window: &MainWindow, folder: PathBuf) {
    let shots = shots();
    let window = window.as_weak();
    let _ = std::fs::create_dir_all(&folder);
    Timer::single_shot(FIRST, move || {
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
                // choosing the language shows the state of the dongle again
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
    });
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
