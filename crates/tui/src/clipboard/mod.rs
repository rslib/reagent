//! The local system clipboard: a copy in the TUI also lands there, beside the
//! OSC 52 sequence the terminal gets.
//!
//! The OSC 52 sequence is the copy the terminal answers for a remote run; this
//! module writes the same text on the machine the process runs on, so a copy in
//! a local run pastes in another application. The write runs on its own thread:
//! a pasteboard call never holds a frame.

/// Where a copy goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    /// Nowhere: a remote run, or an OS this build has no backend for.
    None,
    /// The general pasteboard of macOS.
    Macos,
    /// The Wayland clipboard of Linux.
    Wayland,
    /// The X11 selection of Linux.
    X11,
}

/// The variables an SSH session sets, which mean that no clipboard is local.
const SSH: &[&str] = &["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"];

/// Where a copy goes, from the OS name and a lookup of environment variables.
///
/// `var` says that a variable is set and not empty. A remote run has no local
/// display to copy into, so SSH wins over any of them.
fn route(os: &str, var: impl Fn(&str) -> bool) -> Route {
    if SSH.iter().any(|name| var(name)) {
        return Route::None;
    }
    match os {
        "macos" => Route::Macos,
        "linux" if var("WAYLAND_DISPLAY") => Route::Wayland,
        "linux" if var("DISPLAY") => Route::X11,
        _ => Route::None,
    }
}

/// The route of a Wayland copy that failed: X11 when a display is set.
// No host but Linux has a Wayland clipboard to fall back from, and the tests
// hold the rule on every host.
#[cfg_attr(
    not(all(feature = "clipboard-native", target_os = "linux")),
    allow(dead_code)
)]
fn after_wayland_error(display: bool) -> Route {
    if display { Route::X11 } else { Route::None }
}

/// Whether the environment variable `name` is set and not empty.
fn is_set(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| !value.is_empty())
}

/// The cap on the bytes of a pasted image: 64 MiB.
pub const CAP: usize = 64 * 1024 * 1024;

/// Why a read with no local clipboard answers.
const NOT_LOCAL: &str = "the clipboard is not local";

/// Why a read of an image over [`CAP`] answers.
const OVER: &str = "the image is over 64 MiB";

/// An image the clipboard holds, with the extension of the file it goes in.
#[derive(Debug, Clone)]
pub struct ClipImage {
    /// The image's bytes.
    pub bytes: Vec<u8>,
    /// The extension of the file the bytes belong in: `png` or `tiff`.
    pub ext: &'static str,
}

/// Reads the image of the local clipboard. A blocking call: the app runs it in
/// `spawn_blocking`.
///
/// `Ok(None)` means the clipboard holds no image, which is no error: a paste of
/// no text asks every time. A remote run and a build without the feature have
/// no local clipboard at all, and a read of an image over [`CAP`] fails.
pub fn read_image() -> Result<Option<ClipImage>, String> {
    if !cfg!(feature = "clipboard-native") {
        return Err(NOT_LOCAL.to_owned());
    }
    match route(std::env::consts::OS, is_set) {
        #[cfg(all(feature = "clipboard-native", target_os = "macos"))]
        Route::Macos => macos::read_image(),
        #[cfg(all(feature = "clipboard-native", target_os = "linux"))]
        Route::Wayland => linux::wayland_image(),
        #[cfg(all(feature = "clipboard-native", target_os = "linux"))]
        Route::X11 => linux::x11_image(),
        _ => Err(NOT_LOCAL.to_owned()),
    }
}

/// The image `bytes` of type `ext`: no bytes is no image, and more than [`CAP`]
/// of them is an error.
pub(super) fn capped(bytes: Vec<u8>, ext: &'static str) -> Result<Option<ClipImage>, String> {
    if bytes.is_empty() {
        return Ok(None);
    }
    if bytes.len() > CAP {
        return Err(OVER.to_owned());
    }
    Ok(Some(ClipImage { bytes, ext }))
}

/// Puts `text` in the local system clipboard and returns at once; a failure is
/// silent, because the OSC 52 sequence already went to the terminal.
pub fn copy(text: &str) {
    // A build without the feature writes nothing, and no text is no copy.
    if !cfg!(feature = "clipboard-native") || text.is_empty() {
        return;
    }
    let route = route(std::env::consts::OS, is_set);
    if route == Route::None {
        return;
    }
    let text = text.to_owned();
    let _ = std::thread::Builder::new()
        .name("clipboard".to_owned())
        .spawn(move || put(route, &text));
}

#[cfg(all(feature = "clipboard-native", target_os = "linux"))]
mod linux;
#[cfg(all(feature = "clipboard-native", target_os = "macos"))]
mod macos;

/// Writes `text` through the backend of `route`; a route with no backend on
/// this OS does nothing.
fn put(route: Route, text: &str) {
    match route {
        #[cfg(all(feature = "clipboard-native", target_os = "macos"))]
        Route::Macos => {
            let _ = macos::write(text);
        }
        #[cfg(all(feature = "clipboard-native", target_os = "linux"))]
        Route::Wayland => {
            let display = is_set("DISPLAY");
            let _ = match linux::wayland(text) {
                Err(_) if after_wayland_error(display) == Route::X11 => linux::x11(text),
                answer => answer,
            };
        }
        #[cfg(all(feature = "clipboard-native", target_os = "linux"))]
        Route::X11 => {
            let _ = linux::x11(text);
        }
        // No backend answers this route, and a build without one uses no text.
        _ => {
            let _ = text;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A lookup that has the variables the test names.
    fn has(names: &'static [&'static str]) -> impl Fn(&str) -> bool {
        move |name| names.contains(&name)
    }

    #[test]
    fn route_ssh_connection_has_no_route() {
        let route = route("macos", has(&["SSH_CONNECTION"]));
        assert_eq!(route, Route::None);
    }

    #[test]
    fn route_ssh_client_has_no_route() {
        let route = route("macos", has(&["SSH_CLIENT"]));
        assert_eq!(route, Route::None);
    }

    #[test]
    fn route_ssh_tty_has_no_route() {
        let route = route("linux", has(&["SSH_TTY"]));
        assert_eq!(route, Route::None);
    }

    #[test]
    fn route_ssh_wins_over_a_display() {
        let route = route("linux", has(&["SSH_TTY", "DISPLAY", "WAYLAND_DISPLAY"]));
        assert_eq!(route, Route::None);
    }

    #[test]
    fn route_macos_has_the_pasteboard() {
        let route = route("macos", has(&[]));
        assert_eq!(route, Route::Macos);
    }

    #[test]
    fn route_wayland_has_a_wayland_display() {
        let route = route("linux", has(&["WAYLAND_DISPLAY"]));
        assert_eq!(route, Route::Wayland);
    }

    #[test]
    fn route_wayland_wins_over_x11() {
        let route = route("linux", has(&["WAYLAND_DISPLAY", "DISPLAY"]));
        assert_eq!(route, Route::Wayland);
    }

    #[test]
    fn route_x11_has_a_display() {
        let route = route("linux", has(&["DISPLAY"]));
        assert_eq!(route, Route::X11);
    }

    #[test]
    fn route_linux_with_no_display_has_no_route() {
        let route = route("linux", has(&[]));
        assert_eq!(route, Route::None);
    }

    #[test]
    fn route_another_os_has_no_route() {
        let route = route("freebsd", has(&["DISPLAY"]));
        assert_eq!(route, Route::None);
    }

    #[test]
    fn a_wayland_error_with_a_display_goes_to_x11() {
        assert_eq!(after_wayland_error(true), Route::X11);
    }

    #[test]
    fn a_wayland_error_without_a_display_stops() {
        assert_eq!(after_wayland_error(false), Route::None);
    }
}
