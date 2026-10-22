#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod autostart;
mod controls;
mod instance;
mod lang;
mod platform;
#[cfg(feature = "snapshot")]
mod snapshot;
mod sound;
mod status;
mod throttle;
#[cfg(test)]
mod translations;

mod ui {
    slint::include_modules!();
}

use slint::ComponentHandle;

use crate::instance::Outcome;
use crate::lang::Lang;
use crate::ui::{MainWindow, Tray};

struct Args {
    /// Start in the notification area without showing the window.
    minimized: bool,
    #[cfg(feature = "snapshot")]
    snapshot: Option<std::path::PathBuf>,
}

impl Args {
    fn parse(args: impl IntoIterator<Item = String>) -> Args {
        let args: Vec<String> = args.into_iter().collect();
        Args {
            minimized: args.iter().any(|arg| arg == "--minimized"),
            #[cfg(feature = "snapshot")]
            snapshot: args
                .iter()
                .position(|arg| arg == "--snapshot")
                .and_then(|at| args.get(at + 1))
                .map(Into::into),
        }
    }

    /// Whether the window shows simulated states instead of the dongle (development only).
    fn simulated(&self) -> bool {
        #[cfg(feature = "snapshot")]
        return self.snapshot.is_some();
        #[cfg(not(feature = "snapshot"))]
        false
    }
}

fn main() {
    if let Err(error) = run(Args::parse(std::env::args().skip(1))) {
        platform::show_error(&format!("cxtune could not start: {error}"));
        std::process::exit(1);
    }
}

fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let guard = match instance::acquire()? {
        Outcome::First(guard) => guard,
        Outcome::Second => return Ok(()),
    };

    let window = MainWindow::new()?;
    let tray = Tray::new()?;
    let lang = lang::preferences_path()
        .and_then(|path| lang::load(&path))
        .unwrap_or_else(Lang::of_windows);

    let show = !args.minimized;
    #[cfg(feature = "snapshot")]
    let show = show || args.snapshot.is_some();
    // the volume is read only for a window that shows the real state
    app::install(&window, &tray, lang, &guard, show && !args.simulated());
    if show {
        window.show()?;
    }
    tray.show()?;
    #[cfg(feature = "snapshot")]
    if let Some(folder) = args.snapshot {
        snapshot::schedule(&window, folder);
        slint::run_event_loop_until_quit()?;
        return Ok(());
    }
    app::start();
    slint::run_event_loop_until_quit()?;
    Ok(())
}
