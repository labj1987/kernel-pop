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
//! The running copy answers each request once its window has handled it, and
//! at once with "starting" until its window's first frame is on screen. A
//! later launch that gets no answer in time says the running copy is not
//! responding (an error, with its PID); one that only heard "starting" says
//! the running copy is still starting (information: its window comes up on
//! its own). When the app is done with the socket (its last window closed, or
//! none could be opened) it closes it (`release`), so the next launch starts a
//! new copy at once instead of waiting for one that is going away.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
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
    ui_up: UiUp,
    listening: Option<Listening>,
}

impl Activations {
    fn new(rx: UnboundedReceiver<Activation>, ui_up: UiUp, listening: Option<Listening>) -> Self {
        Activations { rx, ui_up, listening }
    }

    /// Whether this copy holds the single-instance socket, so no other copy
    /// of this login runs (none can start a download, say, behind its back).
    pub fn is_exclusive(&self) -> bool {
        self.listening.is_some()
    }

    /// The next request; `None` once nothing can send one any more.
    pub async fn recv(&mut self) -> Option<Activation> {
        self.rx.recv().await
    }

    #[cfg(test)]
    fn blocking_recv(&mut self) -> Option<Activation> {
        self.rx.blocking_recv()
    }

    /// The handle that says the window is up, set from its first drawn frame.
    pub fn up_signal(&self) -> UiUp {
        self.ui_up.clone()
    }

    #[cfg(test)]
    fn ui_is_up(&self) {
        self.ui_up.set();
    }

    #[cfg(test)]
    fn close(&self) {
        if let Some(listening) = &self.listening {
            listening.close();
        }
    }
}

/// Says the window is up and requests are being handled. Until then the
/// listener answers each request with "starting", so a launch that comes
/// while this copy is still starting (a cold start on a software renderer
/// can take seconds) waits longer instead of calling it hung. Set once the
/// window's first frame has been drawn and presented: a window that exists
/// but was never shown is not up yet.
#[derive(Clone, Default)]
pub struct UiUp(Arc<AtomicBool>);

impl UiUp {
    pub fn set(&self) {
        self.0.store(true, Ordering::Release);
    }

    fn is_set(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// The primary copy's socket, closed when the app is done with it (`release`).
/// Without that, a later launch would connect to a copy that no longer reads
/// its requests, wait out the whole start wait, and call it stuck.
#[derive(Clone)]
struct Listening {
    listener: Arc<UnixListener>,
    path: PathBuf,
    /// The `.alive` lock the AppImage's launcher looks at (see `hold_alive`).
    alive: Option<Arc<std::fs::File>>,
    closed: Arc<AtomicBool>,
}

impl Listening {
    /// Drops the `.alive` lock, removes the socket file, then shuts the
    /// listener down. In that order: the launcher must not see a copy running
    /// once its socket is gone (it would skip its checks for a launch that
    /// then becomes the primary copy itself), and a launch that finds the file
    /// still there gets "refused", which means nobody listens, and binds a new
    /// one. Harmless twice.
    fn close(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(alive) = &self.alive {
            // SAFETY: flock on a file this struct keeps open.
            unsafe { libc::flock(alive.as_raw_fd(), libc::LOCK_UN) };
        }
        let _ = std::fs::remove_file(&self.path);
        // Wakes the listening thread's accept with an error once the backlog
        // is empty, and refuses connections from here on; the thread then ends.
        // SAFETY: shutdown on a socket this struct keeps open.
        unsafe { libc::shutdown(self.listener.as_raw_fd(), libc::SHUT_RDWR) };
    }
}

/// The primary copy's socket, for `release`.
static PRIMARY: std::sync::OnceLock<Listening> = std::sync::OnceLock::new();

/// Stops answering later launches and removes the socket. Call it when the
/// app is about to end (its last window closed, or none could be opened), so
/// the next launch becomes the primary copy at once. Requests already queued
/// get their connection closed when the `Activations` holding them is dropped.
pub fn release() {
    if let Some(listening) = PRIMARY.get() {
        listening.close();
    }
}

/// What the running copy answers once it has handled a request.
const REPLY: &[u8] = b"ok\n";

/// What it answers at once while its window is not up yet.
const STARTING: &[u8] = b"starting\n";

/// How long a later launch waits.
#[derive(Clone, Copy)]
struct Waits {
    /// For any answer at all; also how long a running copy that keeps closing
    /// the connection unanswered (it is exiting) is asked again, counted from
    /// the first time it did.
    first: Duration,
    /// For "ok" from a copy that said it is starting, counted from the request.
    total: Duration,
}

const WAITS: Waits = Waits { first: Duration::from_secs(2), total: Duration::from_secs(15) };

/// How often a copy that closed the connection unanswered is asked again.
const RETRY_STEP: Duration = Duration::from_millis(75);

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
    /// Another copy is running and has shown its window (or copies kept going
    /// away while asked, and this launch leaves quietly).
    HandedOver,
    /// Another copy is running and still starting: it has the request and
    /// shows its window when it is up. Its process id when the socket tells it.
    Starting(Option<i32>),
    /// Another copy is running but did not answer in time (its window is
    /// hung); its process id when the socket tells it.
    NotResponding(Option<i32>),
}

/// `None` when another copy is already running (it has been asked to show
/// its window); otherwise this is the primary copy, and the receiver yields
/// once per later launch. A running copy that is still starting is reported
/// as information and this launch exits with 0; one that does not answer is
/// reported as an error (on stderr and in a desktop message) and this launch
/// exits with 1.
pub fn claim() -> Option<Activations> {
    match runtime_dir() {
        Some(dir) => match claim_at(&dir.join("kernel-pop.sock"), launch_token()) {
            Claim::Primary(activations) => {
                if let Some(listening) = &activations.listening {
                    let _ = PRIMARY.set(listening.clone());
                }
                Some(activations)
            }
            Claim::HandedOver => None,
            Claim::Starting(pid) => {
                let message = starting_message(pid);
                eprintln!("kernel-pop: {message}");
                crate::notice::show_info(&message);
                None
            }
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
            Some(Activations::new(rx, UiUp::default(), None))
        }
    }
}

fn not_responding_message(pid: Option<i32>) -> String {
    let which = pid.map(|pid| format!(" (PID {pid})")).unwrap_or_default();
    format!("Kernel Pop is already running but not responding; close it{which} and start again.")
}

fn starting_message(pid: Option<i32>) -> String {
    let which = pid.map(|pid| format!(" (process {pid})")).unwrap_or_default();
    format!("Kernel Pop is still starting{which}; its window will appear when it is ready.")
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

/// The primary copy holds `flock` on `<socket>.alive` until it releases the
/// socket or ends, so the AppImage's launcher can tell, without connecting,
/// that a copy is running and skip its first-launch setup (`instance_running`
/// in build-appimage.sh). The file stays open with the socket.
fn hold_alive(socket: &Path) -> Option<std::fs::File> {
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(sibling(socket, ".alive")).ok()?;
    // The AppImage's launcher holds it for a moment while it checks; wait
    // that out (up to a second) rather than run without it.
    for _ in 0..100 {
        // SAFETY: flock on a file descriptor this function owns.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Some(file);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    eprintln!("kernel-pop: could not lock {}; the launcher will not see this copy", sibling(socket, ".alive").display());
    None
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

/// The running copy closed the connection without an answer (end of file,
/// a reset or a broken pipe): it was exiting, so the claim is tried again.
struct Closed;

/// Whether a read or write error means the other end went away.
fn gone(error: &std::io::Error) -> bool {
    matches!(error.kind(), std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe)
}

/// Sends the request and waits for the running copy's answer: "ok" within
/// `waits.first`, or "starting" within that and then "ok" within
/// `waits.total` of the request.
fn hand_over(stream: UnixStream, token: Option<&str>, waits: Waits) -> Result<Claim, Closed> {
    let pid = peer_pid(&stream);
    let asked = Instant::now();
    let _ = stream.set_write_timeout(Some(waits.first));
    if let Err(e) = writeln!(&stream, "activate {}", token.unwrap_or("")) {
        return if gone(&e) { Err(Closed) } else { Ok(Claim::NotResponding(pid)) };
    }
    let mut reader = BufReader::new(&stream).take(64);
    let mut line = Vec::new();
    let _ = stream.set_read_timeout(Some(waits.first));
    let mut read = reader.read_until(b'\n', &mut line);
    let starting = read.is_ok() && line == STARTING;
    if starting {
        line.clear();
        let left = waits.total.saturating_sub(asked.elapsed()).max(Duration::from_millis(1));
        let _ = stream.set_read_timeout(Some(left));
        read = reader.read_until(b'\n', &mut line);
    }
    match read {
        _ if line == REPLY => Ok(Claim::HandedOver),
        Ok(0) => Err(Closed),
        Err(e) if gone(&e) => Err(Closed),
        // A timeout, or an answer that is not "ok".
        _ if starting => Ok(Claim::Starting(pid)),
        _ => Ok(Claim::NotResponding(pid)),
    }
}

fn claim_at(path: &Path, token: Option<String>) -> Claim {
    claim_with(path, token, WAITS)
}

/// A copy that exits while it is asked closes the connection unanswered, and
/// its listener may take a moment more to go away. So it is asked again every
/// `RETRY_STEP` until nobody listens (this launch becomes the primary) or a
/// copy answers. If copies keep going away unanswered for `waits.first`,
/// counted from the first closed connection (a copy may have said "starting"
/// for seconds before it went), this launch leaves quietly rather than call
/// one hung.
fn claim_with(path: &Path, token: Option<String>, waits: Waits) -> Claim {
    let mut deadline = None;
    loop {
        if let Ok(claim) = claim_once(path, token.as_deref(), waits) {
            return claim;
        }
        let deadline = *deadline.get_or_insert_with(|| Instant::now() + waits.first);
        if Instant::now() >= deadline {
            return Claim::HandedOver;
        }
        std::thread::sleep(RETRY_STEP);
    }
}

fn claim_once(path: &Path, token: Option<&str>, waits: Waits) -> Result<Claim, Closed> {
    let lock = lock(path);
    match UnixStream::connect(path) {
        Ok(stream) => {
            // Others may look while this one waits for the answer.
            drop(lock);
            return hand_over(stream, token, waits);
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
    let ui_up = UiUp::default();
    let activations = match UnixListener::bind(path) {
        Ok(listener) => {
            let alive = hold_alive(path).map(Arc::new);
            let listening = Listening { listener: Arc::new(listener), path: path.to_path_buf(), alive, closed: Arc::default() };
            let activations = Activations::new(rx, ui_up.clone(), Some(listening.clone()));
            std::thread::spawn(move || listen(&listening, &ui_up, &tx));
            activations
        }
        Err(e) => {
            // Without the socket the app still runs; it just can't be found
            // by a second launch.
            eprintln!("kernel-pop: single-instance socket {}: {e}", path.display());
            std::mem::forget(tx);
            Activations::new(rx, ui_up, None)
        }
    };
    Ok(Claim::Primary(activations))
}

/// The listening thread: one `Activation` per later launch. A connection
/// accepted after `release`, or one the UI no longer takes, is dropped
/// unanswered, so that launch hears the end of the connection and asks again.
/// The loop goes on after one: every launch already waiting in the backlog is
/// taken and dropped the same way, and only the accept error that `release`'s
/// shutdown causes once the backlog is empty ends it.
fn listen(listening: &Listening, ui_up: &UiUp, tx: &tokio::sync::mpsc::UnboundedSender<Activation>) {
    for connection in listening.listener.incoming() {
        let mut connection = match connection {
            Ok(connection) => connection,
            // Shut down by `release`, and the backlog is empty: no more requests.
            Err(_) if listening.closed.load(Ordering::Acquire) => break,
            Err(_) => {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
        };
        // Released between the accept and now: dropped unanswered, so that
        // launch asks again and finds nobody.
        if listening.closed.load(Ordering::Acquire) {
            continue;
        }
        let token = read_request(&connection);
        if !ui_up.is_set() {
            let _ = connection.write_all(STARTING);
        }
        // A UI that no longer takes requests drops this one, unanswered.
        let _ = tx.send(Activation { token, reply: Some(connection) });
    }
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
        claim_at, claim_with, listen, nobody_listening, not_responding_message, parse_request, sibling, starting_message, valid_token, Activations, Claim,
        Listening, UiUp, Waits, MAX_TOKEN, RETRY_STEP, WAITS,
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

    /// Whether `<socket>.alive` is locked by someone (as the launcher checks it).
    fn alive_locked(path: &std::path::Path) -> bool {
        let file = std::fs::File::open(sibling(path, ".alive")).unwrap();
        // SAFETY: flock on a file this test owns; released when it closes.
        (unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }) == -1
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
        drop(hold);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_messages_tell_starting_from_not_responding() {
        assert_eq!(not_responding_message(Some(42)), "Kernel Pop is already running but not responding; close it (PID 42) and start again.");
        assert_eq!(not_responding_message(None), "Kernel Pop is already running but not responding; close it and start again.");
        assert_eq!(starting_message(Some(42)), "Kernel Pop is still starting (process 42); its window will appear when it is ready.");
        assert_eq!(starting_message(None), "Kernel Pop is still starting; its window will appear when it is ready.");
    }

    const SHORT: Waits = Waits { first: Duration::from_millis(300), total: Duration::from_millis(1500) };

    #[test]
    fn the_waits_are_two_seconds_then_fifteen_while_starting() {
        assert_eq!((WAITS.first, WAITS.total), (Duration::from_secs(2), Duration::from_secs(15)));
        assert_eq!(RETRY_STEP, Duration::from_millis(75));
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
    fn a_copy_stuck_in_start_up_is_reported_as_still_starting_after_the_long_wait() {
        let dir = scratch("stuck");
        let path = dir.join("test.sock");
        // Its window never comes up: only "starting", never "ok".
        let _first = primary(claim_with(&path, None, SHORT));
        let started = Instant::now();
        assert!(
            matches!(claim_with(&path, None, SHORT), Claim::Starting(Some(pid)) if pid == std::process::id() as i32),
            "still starting is not 'not responding'"
        );
        let waited = started.elapsed();
        assert!(waited >= Duration::from_millis(1400) && waited < Duration::from_secs(3), "{waited:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn once_the_window_is_up_a_stuck_copy_gets_only_the_short_wait() {
        let dir = scratch("up-stuck");
        let path = dir.join("test.sock");
        let mut first = primary(claim_with(&path, None, SHORT));
        first.ui_is_up();
        let started = Instant::now();
        let launch = {
            let path = path.clone();
            std::thread::spawn(move || claim_with(&path, None, SHORT))
        };
        let request = first.blocking_recv().unwrap();
        assert!(matches!(launch.join().unwrap(), Claim::NotResponding(Some(_))));
        assert!(started.elapsed() < Duration::from_secs(1), "{:?}", started.elapsed());
        drop(request);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_copy_that_exits_while_asked_is_replaced() {
        let dir = scratch("exiting");
        let path = dir.join("test.sock");
        // In an app's order: the connection closes first (unread: a reset),
        // the listener and its socket file a moment later. Asking again at once
        // still reaches that listener's backlog; the launch keeps asking.
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let exiting = {
            let path = path.clone();
            std::thread::spawn(move || {
                let (connection, _) = listener.accept().unwrap();
                drop(connection);
                std::thread::sleep(Duration::from_millis(300));
                let _ = std::fs::remove_file(&path);
                drop(listener);
            })
        };
        let started = Instant::now();
        let claim = claim_with(&path, None, Waits { first: Duration::from_secs(2), total: Duration::from_secs(5) });
        exiting.join().unwrap();
        assert!(matches!(claim, Claim::Primary(ref first) if first.is_exclusive()), "the later launch takes over, not 'not responding'");
        assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_copy_that_goes_away_after_saying_starting_still_gets_the_retries() {
        let dir = scratch("starting-dies");
        let path = dir.join("test.sock");
        // It says "starting", stays silent longer than the first wait, then
        // goes away (connection first, socket a moment later). The retries'
        // time counts from that, not from the request.
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let dying = {
            let path = path.clone();
            std::thread::spawn(move || {
                use std::io::Write;
                let (mut connection, _) = listener.accept().unwrap();
                connection.write_all(b"starting\n").unwrap();
                std::thread::sleep(Duration::from_millis(600));
                drop(connection);
                std::thread::sleep(Duration::from_millis(150));
                let _ = std::fs::remove_file(&path);
                drop(listener);
            })
        };
        let claim = claim_with(&path, None, Waits { first: Duration::from_millis(300), total: Duration::from_secs(5) });
        dying.join().unwrap();
        assert!(matches!(claim, Claim::Primary(_)), "retried after the copy went away, and became the primary");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn copies_that_keep_hanging_up_end_the_launch_quietly_after_the_first_wait() {
        let dir = scratch("flapping");
        let path = dir.join("test.sock");
        // Every attempt finds a copy that takes the request and hangs up, and keeps its socket.
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flapping = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut taken = 0;
                for connection in listener.incoming() {
                    drop(connection);
                    taken += 1;
                    if stop.load(std::sync::atomic::Ordering::Acquire) {
                        break;
                    }
                }
                taken
            })
        };
        let started = Instant::now();
        let claim = claim_with(&path, None, Waits { first: Duration::from_millis(500), total: Duration::from_secs(5) });
        let waited = started.elapsed();
        stop.store(true, std::sync::atomic::Ordering::Release);
        drop(std::os::unix::net::UnixStream::connect(&path));
        let taken = flapping.join().unwrap();
        assert!(matches!(claim, Claim::HandedOver), "quiet, not 'not responding'");
        assert!(taken > 2, "asked again in short steps: {taken}");
        assert!(waited >= Duration::from_millis(500) && waited < Duration::from_secs(2), "{waited:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A primary copy's socket with nobody serving it yet.
    fn bound(path: &std::path::Path) -> Listening {
        Listening {
            listener: std::sync::Arc::new(std::os::unix::net::UnixListener::bind(path).unwrap()),
            path: path.to_path_buf(),
            alive: None,
            closed: Default::default(),
        }
    }

    /// What a launch heard before its connection ended; `None` when it is
    /// still open a second later (nobody took the connection).
    fn heard(mut stream: std::os::unix::net::UnixStream) -> Option<Vec<u8>> {
        use std::io::Read;
        stream.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).ok().map(|_| bytes)
    }

    #[test]
    fn every_launch_queued_at_a_released_socket_hears_the_end_at_once() {
        let dir = scratch("backlog");
        let path = dir.join("test.sock");
        let listening = bound(&path);
        // Three launches wait in the backlog when the copy releases its socket.
        let waiting: Vec<_> = (0..3).map(|_| std::os::unix::net::UnixStream::connect(&path).unwrap()).collect();
        listening.close();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        listen(&listening, &UiUp::default(), &tx);
        for stream in waiting {
            assert_eq!(heard(stream), Some(Vec::new()), "dropped unanswered, so that launch asks again");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_request_the_ui_no_longer_takes_is_dropped_unanswered() {
        use std::io::Write;
        let dir = scratch("no-ui");
        let path = dir.join("test.sock");
        let listening = bound(&path);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        drop(rx);
        let serving = {
            let listening = listening.clone();
            std::thread::spawn(move || listen(&listening, &UiUp::default(), &tx))
        };
        for _ in 0..3 {
            let stream = std::os::unix::net::UnixStream::connect(&path).unwrap();
            writeln!(&stream, "activate").unwrap();
            assert_eq!(heard(stream).as_deref(), Some(&b"starting\n"[..]));
        }
        listening.close();
        serving.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_released_copy_lets_the_next_launch_start_at_once() {
        let dir = scratch("released");
        let path = dir.join("test.sock");
        let first = primary(claim_at(&path, None));
        assert!(alive_locked(&path));
        first.close();
        assert!(!path.exists(), "the socket file is removed");
        assert!(!alive_locked(&path), "and the launcher no longer sees a running copy");
        first.close();
        let error = std::os::unix::net::UnixStream::connect(&path).unwrap_err();
        assert!(nobody_listening(&error), "{error}");
        let started = Instant::now();
        assert!(matches!(claim_at(&path, None), Claim::Primary(_)), "the next launch is the primary");
        assert!(started.elapsed() < Duration::from_secs(1), "{:?}", started.elapsed());
        drop(first);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn launches_queued_at_a_copy_that_gives_up_start_at_once() {
        let dir = scratch("gives-up");
        let path = dir.join("test.sock");
        // The primary's window never comes up (none could be opened).
        let first = primary(claim_at(&path, None));
        let started = Instant::now();
        let launch = {
            let path = path.clone();
            std::thread::spawn(move || claim_at(&path, None))
        };
        // Its request is queued (told "starting"); then the copy gives up as
        // ui::no_window does: release, and drop the requests.
        std::thread::sleep(Duration::from_millis(300));
        first.close();
        drop(first);
        let claim = launch.join().unwrap();
        assert!(matches!(claim, Claim::Primary(_)), "not 'still starting' beside the other copy's error");
        assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_primary_holds_the_alive_lock_for_the_launcher() {
        let dir = scratch("alive");
        let path = dir.join("test.sock");
        let _first = primary(claim_at(&path, None));
        assert!(alive_locked(&path), "a running copy must keep <socket>.alive locked");
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
