mod activation;
mod build;
mod download;
mod install;
mod instance;
mod kernelorg;
mod notice;
mod source;
mod system;
mod ui;
mod verify;

use std::sync::OnceLock;
use tokio::runtime::Runtime;

/// The Wayland app_id and X11 window class. It matches `StartupWMClass` in the
/// .desktop file, so the running window is tied to the desktop file (name,
/// icon) on either backend.
const APP_ID: &str = "io.github.labj1987.KernelPop";

static TOKIO_RT: OnceLock<Runtime> = OnceLock::new();

pub fn runtime() -> &'static Runtime {
    TOKIO_RT.get_or_init(|| tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("Failed to build Tokio runtime"))
}

/// The UI toolkit reports renderer and display-server problems through the
/// `log` crate; they go to stderr so a terminal or the journal shows them.
struct StderrLog;

/// Libraries whose info and debug lines are about their own workings (zbus
/// prints its whole D-Bus handshake, raw bytes and all, at info), not about
/// anything the app or the user can act on: only their warnings and errors
/// are shown, whatever KERNEL_POP_LOG asks for.
const NOISY: [&str; 15] = [
    "zbus_names",
    "zvariant",
    "ashpd",
    "tracing",
    "polling",
    "async_io",
    "async_executor",
    "calloop",
    "mio",
    "reqwest",
    "hyper",
    "hyper_util",
    "h2",
    "rustls",
    "naga",
];

/// Modules (and everything under them) whose warnings are harmless and say
/// nothing the user can act on: only their errors are shown. zbus warns
/// "Failed to populate properties cache" (`zbus::proxy`) when a D-Bus service
/// does not offer the properties interface; GPUI's accessibility tree warns
/// "set_focus called more than once in a single frame" (`gpui::window::a11y`)
/// while a screen reader runs, and goes on with the last focus, as it should.
const ERRORS_ONLY: [&str; 2] = ["zbus", "gpui::window::a11y"];

/// Whether `target` is `module` or inside it (whole path segments only).
fn within(target: &str, module: &str) -> bool {
    target.strip_prefix(module).is_some_and(|rest| rest.is_empty() || rest.starts_with("::"))
}

/// Whether a line at `level` from `target` (a module path) is shown when
/// `max` is asked for.
fn shown(target: &str, level: log::Level, max: log::LevelFilter) -> bool {
    let crate_name = target.split("::").next().unwrap_or(target);
    let max = if ERRORS_ONLY.iter().any(|module| within(target, module)) {
        max.min(log::LevelFilter::Error)
    } else if NOISY.contains(&crate_name) {
        max.min(log::LevelFilter::Warn)
    } else {
        max
    };
    level <= max
}

impl log::Log for StderrLog {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        shown(metadata.target(), metadata.level(), log::max_level())
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            eprintln!("kernel-pop: {} [{}] {}", record.level(), record.target(), record.args());
        }
    }

    fn flush(&self) {}
}

fn main() {
    // Only the primary copy goes on: a second launch has asked the running
    // one to show its window.
    let Some(activations) = instance::claim() else {
        return;
    };
    log::set_logger(&StderrLog).ok();
    // KERNEL_POP_LOG=info (or debug) also shows which renderer was picked.
    let level = std::env::var("KERNEL_POP_LOG").ok().and_then(|level| level.parse().ok()).unwrap_or(log::LevelFilter::Warn);
    log::set_max_level(level);
    let _ = runtime();

    gpui_kit::application().with_assets(ui::Assets).run(move |cx| {
        ui::init(cx);
        ui::build_ui(cx, activations);
    });
}

#[cfg(test)]
mod tests {
    use log::{Level, LevelFilter};

    #[test]
    fn noisy_libraries_show_only_their_warnings() {
        assert!(!super::shown("zvariant::dbus", Level::Debug, LevelFilter::Trace));
        assert!(!super::shown("ashpd::desktop", Level::Info, LevelFilter::Info));
        assert!(super::shown("ashpd", Level::Warn, LevelFilter::Info));
        assert!(super::shown("gpui_wgpu::renderer", Level::Info, LevelFilter::Info), "the renderer choice is still shown at info");
        assert!(!super::shown("gpui", Level::Info, LevelFilter::Warn), "the default stays warnings only");
        assert!(super::shown("gpui::window", Level::Warn, LevelFilter::Warn), "GPUI's own warnings still show");
        assert!(super::shown("zbusy", Level::Info, LevelFilter::Info), "whole crate names only");
    }

    #[test]
    fn harmless_dbus_and_screen_reader_warnings_are_not_shown() {
        // "Failed to populate properties cache via GetAll", from zbus's proxy.
        assert!(!super::shown("zbus::proxy", Level::Warn, LevelFilter::Info));
        assert!(!super::shown("zbus::connection::handshake", Level::Info, LevelFilter::Info));
        assert!(!super::shown("zbus", Level::Debug, LevelFilter::Debug));
        assert!(super::shown("zbus::proxy", Level::Error, LevelFilter::Warn), "errors still show");
        // "a11y: set_focus called more than once in a single frame", with a screen reader.
        assert!(!super::shown("gpui::window::a11y", Level::Warn, LevelFilter::Info));
        assert!(super::shown("gpui::window::a11y", Level::Error, LevelFilter::Warn));
        assert!(super::shown("gpui::window::a11y_extra", Level::Warn, LevelFilter::Warn), "whole module names only");
    }

    #[test]
    fn the_logger_itself_drops_them() {
        use log::Log as _;
        log::set_max_level(LevelFilter::Info);
        let enabled = |target, level| super::StderrLog.enabled(&log::Metadata::builder().target(target).level(level).build());
        assert!(!enabled("zbus::proxy", Level::Warn));
        assert!(!enabled("gpui::window::a11y", Level::Warn));
        assert!(enabled("kernel_pop::ui", Level::Info));
    }
}
