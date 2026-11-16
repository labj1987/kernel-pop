//! notice.rs — telling the user something when the app has no window to say
//! it in: no working graphics driver, or a running copy that does not answer.
//! It goes through whatever the desktop offers, tried in order until one
//! shows it: a dialog (kdialog, zenity), a notification (notify-send, or the
//! notification service over D-Bus with gdbus, which desktops without
//! notify-send such as Pop!_OS COSMIC still have), or xmessage.

/// Shows `message` as an error, when there is a display to show it on.
pub fn show_error(message: &str) {
    if !has_display() {
        return;
    }
    for (program, args) in commands(message) {
        let status = std::process::Command::new(program).args(&args).stdin(std::process::Stdio::null()).status();
        if status.is_ok_and(|status| showed(program, status.code())) {
            break;
        }
    }
}

pub fn has_display() -> bool {
    ["WAYLAND_DISPLAY", "DISPLAY"].into_iter().any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
}

/// The programs tried, in order, with their arguments.
fn commands(message: &str) -> Vec<(&'static str, Vec<String>)> {
    let plain = plain(message);
    let args = |list: &[&str]| list.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
    let with = |mut list: Vec<String>, last: &str| {
        list.push(last.to_string());
        list
    };
    vec![
        ("kdialog", with(args(&["--title", "Kernel Pop", "--error"]), &plain)),
        ("zenity", with(args(&["--error", "--no-markup", "--title=Kernel Pop", "--width=480", "--text"]), message)),
        ("notify-send", with(args(&["--urgency=critical", "--app-name=Kernel Pop", "Kernel Pop"]), &plain)),
        (
            "gdbus",
            vec![
                "call".into(),
                "--session".into(),
                "--dest".into(),
                "org.freedesktop.Notifications".into(),
                "--object-path".into(),
                "/org/freedesktop/Notifications".into(),
                "--method".into(),
                "org.freedesktop.Notifications.Notify".into(),
                gvariant_string("Kernel Pop"),
                "0".into(),
                gvariant_string("dialog-error"),
                gvariant_string("Kernel Pop"),
                gvariant_string(&plain),
                "[]".into(),
                "{'urgency': <byte 2>}".into(),
                "0".into(),
            ],
        ),
        ("xmessage", with(args(&["-center"]), message)),
    ]
}

/// Whether `program`, having ended with exit `code`, did show the message, so
/// the next one must not show it again. The dialogs end with 0 when the user
/// pressed their button and 1 when the user closed them another way; both
/// mean it was seen. Anything else (no display, a crash, a signal) does not.
/// notify-send and gdbus show nothing themselves: only 0 says the desktop
/// took the message. xmessage ends with 1 when it could not open the display.
fn showed(program: &str, code: Option<i32>) -> bool {
    match program {
        "notify-send" | "gdbus" | "xmessage" => code == Some(0),
        _ => matches!(code, Some(0 | 1)),
    }
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
        let names: Vec<_> = commands("x").into_iter().map(|(program, _)| program).collect();
        assert_eq!(names, ["kdialog", "zenity", "notify-send", "gdbus", "xmessage"]);
        assert!(showed("zenity", Some(1)) && !showed("notify-send", Some(1)) && !showed("kdialog", None));
        assert!(showed("gdbus", Some(0)) && !showed("gdbus", Some(1)) && !showed("xmessage", Some(1)));
    }

    #[test]
    fn gdbus_gets_the_message_as_one_quoted_string() {
        let (_, args) = commands("it's \"odd\" <b> & a\\b\nline 2").into_iter().find(|(program, _)| *program == "gdbus").unwrap();
        assert_eq!(args[12], r#""it's \"odd\" ‹b› and a\\b\nline 2""#);
        assert_eq!(args.len(), 16, "Notify takes eight arguments: {args:?}");
    }
}
