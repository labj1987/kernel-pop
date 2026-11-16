//! notice.rs — telling the user something when the app has no window to say
//! it in: no working graphics driver, a running copy that does not answer
//! (errors), or one that is still starting (information).
//! It goes through whatever the desktop offers, tried in order until one
//! shows it: a dialog (zenity, kdialog), a notification (notify-send, or the
//! notification service over D-Bus with `gdbus call`, which desktops without
//! notify-send such as Pop!_OS COSMIC still have), or xmessage.

/// What a message is: an error the user has to act on, or information.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// An error dialog, or a critical notification that stays until closed.
    Error,
    /// An information dialog, or a normal notification with the desktop's
    /// default expiry.
    Info,
}

/// Shows `message` as an error, when there is a display to show it on.
pub fn show_error(message: &str) {
    show(Kind::Error, message);
}

/// Shows `message` as information, when there is a display to show it on.
pub fn show_info(message: &str) {
    show(Kind::Info, message);
}

fn show(kind: Kind, message: &str) {
    if !has_display() {
        return;
    }
    for (program, args) in commands(kind, message) {
        if run(program, &args).is_some_and(|(code, stderr)| showed(program, code, stderr.as_deref())) {
            break;
        }
    }
}

/// Runs `program` with `args`, waits for it, and returns its exit code and
/// what it wrote on stderr. Its stderr goes to a private temporary file
/// (`stderr_file`), read once the program has ended: reading a pipe to its
/// end would also wait for anything the program left running that kept the
/// pipe open. `None` when the program could not be started.
fn run(program: &str, args: &[String]) -> Option<(Option<i32>, Option<String>)> {
    run_with(program, args, stderr_file(&std::env::temp_dir()))
}

/// `run` with stderr going to `file`. Without a file, stderr is unknown
/// (`None`), not empty.
fn run_with(program: &str, args: &[String], file: Option<std::fs::File>) -> Option<(Option<i32>, Option<String>)> {
    let (reader, stderr) = match file.and_then(|file| Some((file.try_clone().ok()?, file))) {
        Some((writer, reader)) => (Some(reader), std::process::Stdio::from(writer)),
        None => (None, std::process::Stdio::null()),
    };
    let status = std::process::Command::new(program).args(args).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(stderr).status();
    let text = reader.and_then(|mut file| {
        use std::io::{Read as _, Seek as _};
        let mut bytes = Vec::new();
        (file.rewind().is_ok() && file.read_to_end(&mut bytes).is_ok()).then(|| String::from_utf8_lossy(&bytes).into_owned())
    });
    status.ok().map(|status| (status.code(), text))
}

/// A file in `dir` for a program's stderr that no other process can know the
/// name of: an unnamed one (`O_TMPFILE`) where the file system has them,
/// otherwise one with a random name (`named_file`). `None` when neither can
/// be made.
fn stderr_file(dir: &std::path::Path) -> Option<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let unnamed = std::fs::OpenOptions::new().read(true).write(true).mode(0o600).custom_flags(libc::O_TMPFILE).open(dir);
    unnamed.ok().or_else(|| named_file(dir))
}

/// A file in `dir` with a random name, removed as soon as it is open (nothing
/// is left behind if this process is killed while the dialog is up); a name
/// that is taken is tried again with another.
fn named_file(dir: &std::path::Path) -> Option<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    for _ in 0..16 {
        let path = dir.join(format!("kernel-pop-notice-{:016x}", random()));
        match std::fs::OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open(&path) {
            Ok(file) => {
                let _ = std::fs::remove_file(&path);
                return Some(file);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => return None,
        }
    }
    None
}

/// 64 unpredictable bits: the standard library's hash keys (seeded from the
/// system's random source) over the clock.
fn random() -> u64 {
    use std::hash::{BuildHasher as _, Hasher as _};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
    hasher.write_u128(now.map_or(0, |now| now.as_nanos()));
    hasher.finish()
}

pub fn has_display() -> bool {
    ["WAYLAND_DISPLAY", "DISPLAY"].into_iter().any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
}

/// The programs tried, in order, with their arguments. An error's
/// notifications have no expiry (0): COSMIC drops one with the default after
/// a few seconds, before anyone has read it. Information gets the normal
/// urgency and the desktop's default expiry (-1).
fn commands(kind: Kind, message: &str) -> Vec<(&'static str, Vec<String>)> {
    let error = kind == Kind::Error;
    let plain = plain(message);
    let args = |list: &[&str]| list.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
    let with = |mut list: Vec<String>, last: &str| {
        list.push(last.to_string());
        list
    };
    let notify_send = if error {
        args(&["--urgency=critical", "--expire-time=0", "--app-name=Kernel Pop", "Kernel Pop"])
    } else {
        args(&["--urgency=normal", "--app-name=Kernel Pop", "Kernel Pop"])
    };
    let mut gdbus = args(&[
        "call",
        "--session",
        "--dest",
        "org.freedesktop.Notifications",
        "--object-path",
        "/org/freedesktop/Notifications",
        "--method",
        "org.freedesktop.Notifications.Notify",
    ]);
    // Notify's eight arguments: app, replaces, icon, summary, body, actions, hints, expiry.
    gdbus.extend([gvariant_string("Kernel Pop"), "uint32 0".into(), gvariant_string(if error { "dialog-error" } else { "dialog-information" })]);
    gdbus.extend([gvariant_string("Kernel Pop"), gvariant_string(&plain), "@as []".into()]);
    gdbus.extend(if error { args(&["{'urgency': <byte 2>}", "int32 0"]) } else { args(&["{'urgency': <byte 1>}", "int32 -1"]) });
    vec![
        ("zenity", with(args(&[if error { "--error" } else { "--info" }, "--no-markup", "--title=Kernel Pop", "--width=480", "--text"]), message)),
        ("kdialog", with(args(&["--title", "Kernel Pop", if error { "--error" } else { "--msgbox" }]), &plain)),
        ("notify-send", with(notify_send, &plain)),
        ("gdbus", gdbus),
        ("xmessage", with(args(&["-center"]), message)),
    ]
}

/// Whether `program`, having ended with exit `code`, did show the message, so
/// the next one must not show it again. The dialogs end with 0 when the user
/// pressed their button and 1 when the user closed them another way; both
/// mean it was seen, unless the dialog says on `stderr` that it could not open
/// the display (zenity then ends with 1 too). Anything else (a crash, a
/// signal) does not. notify-send and gdbus show nothing themselves: only 0
/// says the desktop took the message. xmessage ends with 1 when it could not
/// open the display. With `stderr` unknown (`None`), a dialog's 1 is not
/// taken as seen.
fn showed(program: &str, code: Option<i32>, stderr: Option<&str>) -> bool {
    match (program, stderr) {
        ("notify-send" | "gdbus" | "xmessage", _) => code == Some(0),
        (_, None) => code == Some(0),
        (_, Some(stderr)) if no_display(stderr) => false,
        _ => matches!(code, Some(0 | 1)),
    }
}

/// Whether a dialog program's `stderr` says it could not open the display.
fn no_display(stderr: &str) -> bool {
    ["Failed to open display", "cannot open display", "Cannot open display"].iter().any(|text| stderr.contains(text))
}

/// `text` with nothing kdialog (Qt rich text) or a notification server (body
/// markup) would take as markup.
fn plain(text: &str) -> String {
    text.replace('&', "and").replace('<', "‹").replace('>', "›")
}

/// `text` as a GVariant text-format string, the way gdbus reads its arguments.
fn gvariant_string(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_way_to_show_a_message_is_tried_in_order() {
        for kind in [Kind::Error, Kind::Info] {
            let names: Vec<_> = commands(kind, "x").into_iter().map(|(program, _)| program).collect();
            assert_eq!(names, ["zenity", "kdialog", "notify-send", "gdbus", "xmessage"]);
        }
        let names: Vec<_> = commands(Kind::Error, "x").into_iter().map(|(program, _)| program).collect();
        // The AppRun's libcheck_show tries them in the same order.
        let script = include_str!("../build-appimage.sh");
        let show = &script[script.find("libcheck_show() {").unwrap()..];
        let show = &show[..show.find("\n}\n").unwrap()];
        let order: Vec<usize> = names.iter().map(|name| show.find(&format!("command -v {name} ")).expect(name)).collect();
        assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{order:?}");
    }

    #[test]
    fn only_a_shown_message_stops_the_chain() {
        let shown = |program, code| showed(program, code, Some(""));
        assert!(shown("zenity", Some(1)) && !shown("notify-send", Some(1)) && !shown("kdialog", None));
        assert!(shown("gdbus", Some(0)) && !shown("gdbus", Some(1)) && !shown("xmessage", Some(1)));
        assert!(!showed("zenity", Some(1), Some("(zenity:42): Gtk-WARNING **: cannot open display: :9999\n")), "zenity with no display exits 1 too");
        assert!(!showed("zenity", Some(1), Some("Failed to open display\n")));
        assert!(!showed("kdialog", Some(1), Some("qt.qpa.xcb: could not connect to display\nCannot open display\n")));
    }

    fn arguments(kind: Kind, program: &str) -> Vec<String> {
        commands(kind, "x").into_iter().find(|(name, _)| *name == program).unwrap().1
    }

    #[test]
    fn an_error_is_an_error_dialog_or_a_notification_that_stays() {
        assert_eq!(arguments(Kind::Error, "zenity")[0], "--error");
        assert!(arguments(Kind::Error, "kdialog").contains(&"--error".to_string()));
        let notify_send = arguments(Kind::Error, "notify-send");
        assert!(notify_send.contains(&"--urgency=critical".to_string()) && notify_send.contains(&"--expire-time=0".to_string()), "{notify_send:?}");
        let gdbus = arguments(Kind::Error, "gdbus");
        assert_eq!(gdbus[gdbus.len() - 2..], ["{'urgency': <byte 2>}", "int32 0"]);
    }

    #[test]
    fn information_is_an_info_dialog_or_a_normal_notification() {
        assert_eq!(arguments(Kind::Info, "zenity")[0], "--info");
        let kdialog = arguments(Kind::Info, "kdialog");
        assert!(kdialog.contains(&"--msgbox".to_string()) && !kdialog.contains(&"--error".to_string()), "{kdialog:?}");
        let notify_send = arguments(Kind::Info, "notify-send");
        assert!(notify_send.contains(&"--urgency=normal".to_string()), "{notify_send:?}");
        assert!(!notify_send.iter().any(|arg| arg.starts_with("--expire-time")), "the desktop's default expiry: {notify_send:?}");
        let gdbus = arguments(Kind::Info, "gdbus");
        assert_eq!(gdbus[gdbus.len() - 2..], ["{'urgency': <byte 1>}", "int32 -1"]);
    }

    #[test]
    fn gdbus_gets_the_message_as_one_quoted_string() {
        let (_, args) = commands(Kind::Error, "it's \"odd\" <b> & a\\b\nline 2").into_iter().find(|(program, _)| *program == "gdbus").unwrap();
        assert_eq!(args.len(), 16, "Notify takes eight arguments: {args:?}");
        assert_eq!(args[8..11], [r#""Kernel Pop""#, "uint32 0", r#""dialog-error""#]);
        assert_eq!(args[12], r#""it's \"odd\" ‹b› and a\\b\nline 2""#);
        assert_eq!(args[13], "@as []");
    }

    /// A program on a private PATH: none of these tests may reach a real dialog or display. It is
    /// run through `sh`: executing a file just written can fail with "text file busy" while another
    /// test's child still holds it open for writing.
    fn stub(dir: &std::path::Path, name: &str, body: &str) -> String {
        use std::os::unix::fs::PermissionsExt as _;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.display().to_string()
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("kernel-pop-notice-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_dialogs_stderr_is_read_after_it_ends_even_if_a_child_keeps_it_open() {
        let dir = scratch("child");
        // A "dialog" that leaves a process holding its stderr: a pipe read to
        // its end would wait the 5 s that process sleeps.
        let program = stub(&dir, "zenity", "echo 'cannot open display: :9999' >&2\nsleep 5 &\nexit 1");
        let started = std::time::Instant::now();
        let (code, stderr) = run("sh", &[program]).unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(3), "{:?}", started.elapsed());
        assert_eq!(code, Some(1));
        assert!(!showed("zenity", code, stderr.as_deref()), "{stderr:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_dialogs_stderr_is_read_whatever_its_bytes() {
        let dir = scratch("bytes");
        let program = stub(&dir, "zenity", "printf '\\377\\376 Failed to open display\\n' >&2\nexit 1");
        let (code, stderr) = run("sh", &[program]).unwrap();
        assert_eq!(code, Some(1));
        let stderr = stderr.unwrap();
        assert!(stderr.contains("Failed to open display"), "{stderr:?}");
        assert!(!showed("zenity", code, Some(&stderr)));
        assert!(run(&dir.join("missing").display().to_string(), &[]).is_none(), "a program that is not there");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_named_stderr_file_is_gone_as_soon_as_it_is_open() {
        let dir = std::env::temp_dir().join(format!("kernel-pop-notice-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A file system without unnamed files gets the random names.
        for _ in 0..3 {
            let (_, stderr) = run_with("sh", &["-c".to_string(), "echo seen >&2".to_string()], named_file(&dir)).unwrap();
            assert_eq!(stderr.as_deref(), Some("seen\n"));
        }
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0, "nothing left behind");
        let _ = std::fs::remove_dir_all(&dir);
        assert_ne!(random(), random());
    }

    #[test]
    fn a_dialog_whose_stderr_could_not_be_kept_does_not_stop_the_chain() {
        // Nowhere to put its stderr: a dialog's exit 1 may be "no display".
        let nowhere = std::path::Path::new("/nonexistent/kernel-pop");
        assert!(stderr_file(nowhere).is_none() && named_file(nowhere).is_none());
        let (code, stderr) = run_with("sh", &["-c".to_string(), "exit 1".to_string()], None).unwrap();
        assert_eq!((code, stderr.as_deref()), (Some(1), None));
        assert!(!showed("zenity", code, None), "the next program is tried");
        assert!(!showed("kdialog", code, None));
        assert!(showed("zenity", Some(0), None), "0 is pressed, whatever stderr said");
    }
}
