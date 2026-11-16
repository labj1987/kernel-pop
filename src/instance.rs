//! instance.rs — one running copy of the app per user login.
//!
//! The first copy listens on a socket in the user's runtime directory
//! (`XDG_RUNTIME_DIR`, one per login, shared by every display of that login).
//! A second launch finds it, hands over its activation token so the running
//! window can be raised, and exits: two copies could start two builds or two
//! privileged runs at once, which the one busy state of a window prevents.
//!
//! Without `XDG_RUNTIME_DIR` there is no directory that is known to be the
//! user's alone, so the app runs without the single-instance check rather
//! than trust a socket in a shared place.
//!
//! The running copy answers each request once its window has handled it. A
//! later launch that gets no answer in time says the running copy is not
//! responding (with its PID) instead of exiting without a word.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

/// One request from a later launch to show the running window.
pub struct Activation {
    /// The activation token of that launch, when it had one (see `launch_token`).
    pub token: Option<String>,
    reply: Option<UnixStream>,
}

impl Activation {
    /// Tells the later launch that the request was handled, so it exits
    /// quietly. Sent from the UI loop: a running copy whose window hangs
    /// never sends it, and the later launch says so (see `NotResponding`).
    pub fn handled(mut self) {
        if let Some(mut stream) = self.reply.take() {
            let _ = stream.write_all(REPLY);
        }
    }
}

/// Requests from later launches to show the running window.
pub struct Activations {
    rx: UnboundedReceiver<Activation>,
    ui_up: Arc<AtomicBool>,
}

impl Activations {
    fn new(rx: UnboundedReceiver<Activation>) -> (Self, Arc<AtomicBool>) {
        let ui_up = Arc::new(AtomicBool::new(false));
        (Activations { rx, ui_up: ui_up.clone() }, ui_up)
    }

    /// The next request; `None` once nothing can send one any more.
    pub async fn recv(&mut self) -> Option<Activation> {
        self.rx.recv().await
    }

    #[cfg(test)]
    fn blocking_recv(&mut self) -> Option<Activation> {
        self.rx.blocking_recv()
    }

    /// The window is up and requests are being handled. Until then the
    /// listener answers each request with "starting", so a launch that comes
    /// while this copy is still starting (a cold start on a software renderer
    /// can take seconds) waits longer instead of calling it hung.
    pub fn ui_is_up(&self) {
        self.ui_up.store(true, Ordering::Relaxed);
    }
}

/// What the running copy answers once it has handled a request.
const REPLY: &[u8] = b"ok\n";

/// What it answers at once while its window is not up yet.
const STARTING: &[u8] = b"starting\n";

/// How long a later launch waits for any answer, and, after "starting", for
/// the "ok" in all.
#[derive(Clone, Copy)]
struct Waits {
    first: Duration,
    total: Duration,
}

const WAITS: Waits = Waits { first: Duration::from_secs(2), total: Duration::from_secs(15) };

fn runtime_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR").filter(|dir| !dir.is_empty()).map(PathBuf::from)
}

/// What the desktop gave this launch to raise a window with: the Wayland
/// activation token, or the startup id (which desktops also fill with that
/// token).
fn launch_token() -> Option<String> {
    ["XDG_ACTIVATION_TOKEN", "DESKTOP_STARTUP_ID"].into_iter().filter_map(|name| std::env::var(name).ok()).find(|token| valid_token(token))
}

/// The longest token passed on. Real ones are a few dozen bytes; a Wayland
/// message holds at most 4096, and a request larger than that makes the
/// compositor drop the app's whole connection.
const MAX_TOKEN: usize = 256;

/// A token is a short run of printable ASCII: it travels as one line here
/// and as a string argument to the compositor, and comes from the
/// environment, which anything may have set.
fn valid_token(token: &str) -> bool {
    !token.is_empty() && token.len() <= MAX_TOKEN && token.bytes().all(|b| b.is_ascii_graphic())
}

/// How a launch went with the single-instance socket.
enum Claim {
    /// This is the primary copy; the receiver yields once per later launch.
    Primary(Activations),
    /// Another copy is running and has shown its window.
    HandedOver,
    /// Another copy is running but did not answer in time (its window is
    /// hung); its process id when the socket tells it.
    NotResponding(Option<i32>),
}

/// `None` when another copy is already running (it has been asked to show
/// its window); otherwise this is the primary copy, and the receiver yields
/// once per later launch. A running copy that does not answer is reported
/// (on stderr and in a desktop message) and this launch exits with 1.
pub fn claim() -> Option<Activations> {
    match runtime_dir() {
        Some(dir) => match claim_at(&dir.join("kernel-pop.sock"), launch_token()) {
            Claim::Primary(activations) => Some(activations),
            Claim::HandedOver => None,
            Claim::NotResponding(pid) => {
                let message = not_responding_message(pid);
                eprintln!("kernel-pop: {message}");
                crate::notice::show_error(&message);
                std::process::exit(1);
            }
        },
        None => {
            eprintln!("kernel-pop: XDG_RUNTIME_DIR is not set, so a second launch can't find this one");
            let (tx, rx) = unbounded_channel();
            // Kept open for the life of the process: the receiver never ends.
            std::mem::forget(tx);
            Some(Activations::new(rx).0)
        }
    }
}

fn not_responding_message(pid: Option<i32>) -> String {
    let which = pid.map(|pid| format!(" (PID {pid})")).unwrap_or_default();
    format!("Kernel Pop is already running but not responding; close it{which} and start again.")
}

/// Holds `flock` on the lock file next to the socket, so that of two copies
/// started together only one looks at, removes and binds the socket at a time.
struct Lock(#[allow(dead_code)] std::fs::File);

fn sibling(socket: &Path, suffix: &str) -> PathBuf {
    let mut name = socket.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

fn lock(socket: &Path) -> Option<Lock> {
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(sibling(socket, ".lock")).ok()?;
    // SAFETY: flock on a file descriptor this function owns; released when the file closes.
    (unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0).then_some(Lock(file))
}

/// The primary copy holds `flock` on `<socket>.alive` for as long as it runs,
/// so the AppImage's launcher can tell, without connecting, that a copy is
/// running and skip its first-launch setup (`instance_running` in
/// build-appimage.sh). The file stays open, and locked, until the process ends.
fn hold_alive(socket: &Path) {
    let Ok(file) = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(sibling(socket, ".alive")) else {
        return;
    };
    // SAFETY: flock on a file descriptor this function owns; the file is leaked below.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        std::mem::forget(file);
    }
}

/// Whether a failed connect means "nobody is listening" (a socket file left
/// by a copy that is gone, or no file at all), as opposed to a running copy
/// that could not be reached just now.
fn nobody_listening(error: &std::io::Error) -> bool {
    matches!(error.kind(), std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound)
}

/// The process id at the other end of a Unix socket.
fn peer_pid(stream: &UnixStream) -> Option<i32> {
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: SO_PEERCRED fills a ucred of the given length on a socket this function borrows.
    let rc = unsafe { libc::getsockopt(stream.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, (&raw mut cred).cast(), &raw mut len) };
    (rc == 0 && cred.pid > 0).then_some(cred.pid)
}

/// After sending a request: waits `waits.first` for the running copy's
/// answer; after "starting", up to `waits.total` in all for its "ok".
fn await_reply(stream: UnixStream, waits: Waits) -> Claim {
    let started = std::time::Instant::now();
    let pid = peer_pid(&stream);
    let mut reader = BufReader::new(&stream);
    let mut wait = waits.first;
    loop {
        let _ = stream.set_read_timeout(Some(wait));
        let mut line = Vec::new();
        let _ = (&mut reader).take(64).read_until(b'\n', &mut line);
        if line == REPLY {
            return Claim::HandedOver;
        }
        let left = waits.total.saturating_sub(started.elapsed());
        if line != STARTING || left.is_zero() {
            return Claim::NotResponding(pid);
        }
        wait = left;
    }
}

fn claim_at(path: &Path, token: Option<String>) -> Claim {
    claim_with(path, token, WAITS)
}

fn claim_with(path: &Path, token: Option<String>, waits: Waits) -> Claim {
    let lock = lock(path);
    match UnixStream::connect(path) {
        Ok(mut stream) => {
            let _ = writeln!(stream, "activate {}", token.as_deref().unwrap_or(""));
            // Others may look while this one waits for the answer.
            drop(lock);
            return await_reply(stream, waits);
        }
        Err(e) if nobody_listening(&e) => {
            let _ = std::fs::remove_file(path);
        }
        Err(e) => {
            // Something answers to that name but not to us: leave it alone and
            // run without the check.
            eprintln!("kernel-pop: single-instance socket {}: {e}", path.display());
        }
    }
    let (tx, rx) = unbounded_channel();
    let (activations, ui_up) = Activations::new(rx);
    match UnixListener::bind(path) {
        Ok(listener) => {
            hold_alive(path);
            std::thread::spawn(move || {
                for mut connection in listener.incoming().flatten() {
                    let token = read_request(&connection);
                    if !ui_up.load(Ordering::Relaxed) {
                        let _ = connection.write_all(STARTING);
                    }
                    if tx.send(Activation { token, reply: Some(connection) }).is_err() {
                        break;
                    }
                }
            });
        }
        Err(e) => {
            // Without the socket the app still runs; it just can't be found
            // by a second launch.
            eprintln!("kernel-pop: single-instance socket {}: {e}", path.display());
            std::mem::forget(tx);
        }
    }
    Claim::Primary(activations)
}

/// The token a second launch sent with its request, if any. A launch that
/// sends nothing readable still counts as a request.
fn read_request(connection: &UnixStream) -> Option<String> {
    let _ = connection.set_read_timeout(Some(Duration::from_secs(2)));
    let mut line = String::new();
    let _ = BufReader::new(connection).take(8192).read_line(&mut line);
    parse_request(&line)
}

fn parse_request(line: &str) -> Option<String> {
    let token = line.trim_end_matches('\n').strip_prefix("activate ")?;
    valid_token(token).then(|| token.to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        claim_at, claim_with, nobody_listening, not_responding_message, parse_request, sibling, valid_token, Activations, Claim, Waits, MAX_TOKEN, WAITS,
    };
    use std::os::fd::AsRawFd;
    use std::time::{Duration, Instant};

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("kernel-pop-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn primary(claim: Claim) -> Activations {
        match claim {
            Claim::Primary(rx) => rx,
            _ => panic!("expected the primary copy"),
        }
    }

    /// Answers every request like the UI loop does, passing on the tokens.
    fn answer(mut first: Activations) -> std::sync::mpsc::Receiver<Option<String>> {
        first.ui_is_up();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            while let Some(activation) = first.blocking_recv() {
                let _ = tx.send(activation.token.clone());
                activation.handled();
            }
        });
        rx
    }

    #[test]
    fn second_launch_reaches_the_first_with_its_token() {
        let dir = scratch("instance");
        let path = dir.join("test.sock");
        let tokens = answer(primary(claim_at(&path, None)));
        assert!(matches!(claim_at(&path, Some("token-1".into())), Claim::HandedOver), "a second copy finds the first and stands down");
        assert_eq!(tokens.recv().unwrap(), Some("token-1".to_string()), "and the first is told, with the token to raise its window with");
        assert!(matches!(claim_at(&path, None), Claim::HandedOver));
        assert_eq!(tokens.recv().unwrap(), None, "no token: still a request");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_running_copy_that_never_answers_is_reported_with_its_pid() {
        let dir = scratch("hung");
        let path = dir.join("test.sock");
        // A copy whose window hangs: its socket takes the request, nobody answers.
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let hold = std::thread::spawn(move || {
            let connection = listener.accept();
            std::thread::sleep(std::time::Duration::from_secs(4));
            drop(connection);
        });
        let started = std::time::Instant::now();
        match claim_at(&path, None) {
            Claim::NotResponding(pid) => assert_eq!(pid, Some(std::process::id() as i32)),
            _ => panic!("a silent copy must not count as handed over"),
        }
        let waited = started.elapsed();
        assert!(waited >= std::time::Duration::from_millis(1900) && waited < std::time::Duration::from_secs(4), "{waited:?}");
        assert_eq!(not_responding_message(Some(42)), "Kernel Pop is already running but not responding; close it (PID 42) and start again.");
        drop(hold);
        let _ = std::fs::remove_dir_all(&dir);
    }

    const SHORT: Waits = Waits { first: Duration::from_millis(300), total: Duration::from_millis(1500) };

    #[test]
    fn the_waits_are_two_seconds_then_fifteen_while_starting() {
        assert_eq!((WAITS.first, WAITS.total), (Duration::from_secs(2), Duration::from_secs(15)));
    }

    #[test]
    fn a_launch_during_start_up_waits_for_the_window() {
        let dir = scratch("starting");
        let path = dir.join("test.sock");
        // The window comes up after the first wait has passed: "starting" keeps the later launch waiting.
        let mut first = primary(claim_with(&path, None, SHORT));
        let ui = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(700));
            first.ui_is_up();
            let activation = first.blocking_recv().unwrap();
            activation.handled();
            first
        });
        let started = Instant::now();
        assert!(matches!(claim_with(&path, None, SHORT), Claim::HandedOver));
        assert!(started.elapsed() >= Duration::from_millis(600));
        let _first = ui.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_copy_stuck_in_start_up_is_reported_after_the_long_wait() {
        let dir = scratch("stuck");
        let path = dir.join("test.sock");
        // Its window never comes up: only "starting", never "ok".
        let _first = primary(claim_with(&path, None, SHORT));
        let started = Instant::now();
        assert!(matches!(claim_with(&path, None, SHORT), Claim::NotResponding(Some(_))));
        let waited = started.elapsed();
        assert!(waited >= Duration::from_millis(1400) && waited < Duration::from_secs(3), "{waited:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_primary_holds_the_alive_lock_for_the_launcher() {
        let dir = scratch("alive");
        let path = dir.join("test.sock");
        let _first = primary(claim_at(&path, None));
        let file = std::fs::File::open(sibling(&path, ".alive")).unwrap();
        // SAFETY: flock on a file this test owns.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(rc, -1, "a running copy must keep <socket>.alive locked");
        // And the launcher's own check (flock(1), as build-appimage.sh runs it) sees it.
        if std::process::Command::new("flock").arg("-V").output().is_ok() {
            let out = std::process::Command::new("flock").args(["-n", "-E", "75"]).arg(sibling(&path, ".alive")).arg("true").status().unwrap();
            assert_eq!(out.code(), Some(75));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_socket_file_is_replaced() {
        let dir = scratch("stale");
        let path = dir.join("test.sock");
        // A socket nobody listens on any more.
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        assert!(path.exists());
        // Another test forking a process at that moment holds the listener
        // until its exec; wait until the socket is really dead.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::os::unix::net::UnixStream::connect(&path).is_ok() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let tokens = answer(primary(claim_at(&path, None)));
        assert!(matches!(claim_at(&path, None), Claim::HandedOver), "and the new one answers");
        assert_eq!(tokens.recv().unwrap(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_a_dead_socket_is_removed() {
        use std::io::{Error, ErrorKind};
        assert!(nobody_listening(&Error::from(ErrorKind::ConnectionRefused)));
        assert!(nobody_listening(&Error::from(ErrorKind::NotFound)));
        assert!(!nobody_listening(&Error::from(ErrorKind::PermissionDenied)));
        assert!(!nobody_listening(&Error::from(ErrorKind::WouldBlock)));
        // A path that is not a socket at all is not ours to delete.
        let dir = scratch("notsock");
        let path = dir.join("test.sock");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("keep"), b"x").unwrap();
        let _ = claim_at(&path, None);
        assert!(path.join("keep").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_copies_started_together_leave_one_primary() {
        let dir = scratch("race");
        let path = dir.join("test.sock");
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || claim_with(&path, None, SHORT))
            })
            .collect();
        // Kept until counted: a primary whose receiver is gone stops listening.
        let claims: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(claims.iter().filter(|c| matches!(c, Claim::Primary(_))).count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn requests_and_tokens_are_one_clean_line() {
        assert_eq!(parse_request("activate abc-123_x\n"), Some("abc-123_x".into()));
        assert_eq!(parse_request("activate \n"), None);
        assert_eq!(parse_request("activate\n"), None);
        assert_eq!(parse_request(""), None);
        assert_eq!(parse_request("something else\n"), None);
        assert!(!valid_token("a\nb") && !valid_token("") && !valid_token("a\u{7}"));
        assert!(!valid_token("a b") && !valid_token("tok\u{e9}n"), "printable ASCII only");
        assert!(valid_token(&"x".repeat(MAX_TOKEN)));
        assert!(!valid_token(&"x".repeat(MAX_TOKEN + 1)), "an oversized token must never reach the compositor");
        let long = format!("activate {}\n", "x".repeat(MAX_TOKEN + 1));
        assert_eq!(parse_request(&long), None);
    }
}
