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
const NOISY: [&str; 12] = ["zbus", "tracing", "polling", "async_io", "async_executor", "calloop", "mio", "reqwest", "hyper", "hyper_util", "h2", "rustls"];

/// Whether a line at `level` from `target` is shown when `max` is asked for.
fn shown(target: &str, level: log::Level, max: log::LevelFilter) -> bool {
    let crate_name = target.split("::").next().unwrap_or(target);
    let max = if NOISY.contains(&crate_name) { max.min(log::LevelFilter::Warn) } else { max };
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
        assert!(!super::shown("zbus::connection::handshake", Level::Info, LevelFilter::Info));
        assert!(!super::shown("zbus", Level::Debug, LevelFilter::Debug));
        assert!(super::shown("zbus::connection", Level::Warn, LevelFilter::Info));
        assert!(super::shown("gpui_wgpu::renderer", Level::Info, LevelFilter::Info), "the renderer choice is still shown at info");
        assert!(!super::shown("gpui", Level::Info, LevelFilter::Warn), "the default stays warnings only");
        assert!(!super::shown("zbusy", Level::Info, LevelFilter::Warn));
    }
}
