//! The Linux clipboard: the Wayland data device, or the X11 CLIPBOARD
//! selection.

use wl_clipboard_rs::copy::{ClipboardType, MimeType, Options, Source};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::Event;
use x11rb::protocol::xproto;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode,
    SELECTION_NOTIFY_EVENT, SelectionNotifyEvent, SelectionRequestEvent, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use super::{ClipImage, capped};

/// The bytes the X11 backend keeps of a request for its own header. It has no
/// INCR path, so it refuses a longer text instead.
const REQUEST_MARGIN: usize = 1024;

/// The image the Wayland clipboard holds, as its PNG.
///
/// A tool the machine does not have, or one that fails, holds no image.
pub fn wayland_image() -> Result<Option<ClipImage>, String> {
    match read("wl-paste", &["--no-newline", "--type", "image/png"]) {
        Some(bytes) => capped(bytes, "png"),
        None => Ok(None),
    }
}

/// The image the X11 CLIPBOARD selection holds, as its PNG.
pub fn x11_image() -> Result<Option<ClipImage>, String> {
    let args = ["-selection", "clipboard", "-t", "image/png", "-o"];
    match read("xclip", &args) {
        Some(bytes) => capped(bytes, "png"),
        None => Ok(None),
    }
}

/// Runs `program` with `args` and answers its output: a tool that is not there,
/// or that exits with a failure, holds no image.
#[expect(clippy::disallowed_types, reason = "a blocking clipboard read")]
fn read(program: &str, args: &[&str]) -> Option<Vec<u8>> {
    let output = std::process::Command::new(program)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(output.stdout)
}

/// Puts `text` in the clipboard of a Wayland compositor.
///
/// The call owns the selection and returns when another client takes it, so it
/// runs on a thread of its own.
///
/// An error is from the setup only. A later error in the serve is not one: the
/// text was on the clipboard, and a fallback now would put stale text back.
pub fn wayland(text: &str) -> Result<(), String> {
    let mut options = Options::new();
    // Foreground serves on this thread. The default forks a process of its own,
    // which would outlive the TUI.
    options.foreground(true).clipboard(ClipboardType::Regular);
    let prepared = options
        .prepare_copy(Source::Bytes(text.as_bytes().into()), MimeType::Text)
        .map_err(|error| format!("the Wayland clipboard: {error}"))?;
    let _ = prepared.serve();
    Ok(())
}

/// Puts `text` in the X11 CLIPBOARD selection.
///
/// The call owns the selection and returns when another client takes it, so it
/// runs on a thread of its own.
pub fn x11(text: &str) -> Result<(), String> {
    let (conn, screen) =
        x11rb::connect(None).map_err(|error| format!("the X11 display: {error}"))?;
    let limit = conn.maximum_request_bytes().saturating_sub(REQUEST_MARGIN);
    if text.len() >= limit {
        return Err(format!(
            "the X11 clipboard serves {limit} bytes at most, not {}",
            text.len()
        ));
    }
    let atoms = Atoms::intern(&conn)?;
    let root = conn.setup().roots[screen].root;
    let window = conn
        .generate_id()
        .map_err(|error| format!("an X11 window: {error}"))?;
    xproto::create_window(
        &conn,
        x11rb::COPY_DEPTH_FROM_PARENT,
        window,
        root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_ONLY,
        0,
        &CreateWindowAux::new(),
    )
    .map_err(|error| format!("an X11 window: {error}"))?;
    xproto::set_selection_owner(&conn, window, atoms.clipboard, x11rb::CURRENT_TIME)
        .map_err(|error| format!("the X11 selection: {error}"))?
        .check()
        .map_err(|error| format!("the X11 selection: {error}"))?;
    conn.flush()
        .map_err(|error| format!("the X11 display: {error}"))?;
    let owner = xproto::get_selection_owner(&conn, atoms.clipboard)
        .map_err(|error| format!("the X11 selection: {error}"))?
        .reply()
        .map_err(|error| format!("the X11 selection: {error}"))?;
    if owner.owner != window {
        return Err("another client took the X11 selection".to_owned());
    }
    serve_x11(&conn, window, &atoms, text)
}

/// Serves the CLIPBOARD selection `window` owns until another client takes it.
fn serve_x11(
    conn: &RustConnection,
    window: Window,
    atoms: &Atoms,
    text: &str,
) -> Result<(), String> {
    loop {
        let event = conn
            .wait_for_event()
            .map_err(|error| format!("the X11 display: {error}"))?;
        match event {
            Event::SelectionClear(event) if event.owner == window => return Ok(()),
            Event::SelectionRequest(event) if event.owner == window => {
                answer(conn, atoms, &event, text);
            }
            _ => {}
        }
    }
}

/// Answers one SelectionRequest with the targets served, the text itself or a
/// refusal.
///
/// A requestor that went away fails its own write, and that must not end the
/// loop, so the writes go unchecked.
fn answer(conn: &RustConnection, atoms: &Atoms, request: &SelectionRequestEvent, text: &str) {
    // A client that predates ICCCM sends no property and means the target.
    let property = if request.property == x11rb::NONE {
        request.target
    } else {
        request.property
    };
    let served = if request.target == atoms.targets {
        let targets = [
            atoms.targets,
            atoms.utf8_string,
            AtomEnum::STRING.into(),
            atoms.text,
        ];
        let _ = conn.change_property32(
            PropMode::REPLACE,
            request.requestor,
            property,
            AtomEnum::ATOM,
            &targets,
        );
        true
    } else if request.target == atoms.utf8_string
        || request.target == atoms.text
        || request.target == u32::from(AtomEnum::STRING)
    {
        // Text answers as UTF8_STRING whatever the name of the target.
        let _ = conn.change_property8(
            PropMode::REPLACE,
            request.requestor,
            property,
            atoms.utf8_string,
            text.as_bytes(),
        );
        true
    } else {
        false
    };
    let notify = SelectionNotifyEvent {
        response_type: SELECTION_NOTIFY_EVENT,
        sequence: 0,
        time: request.time,
        requestor: request.requestor,
        selection: request.selection,
        target: request.target,
        property: if served { property } else { x11rb::NONE },
    };
    let _ = conn.send_event(false, request.requestor, EventMask::NO_EVENT, notify);
    let _ = conn.flush();
}

/// The atoms a CLIPBOARD owner answers by.
struct Atoms {
    clipboard: Atom,
    utf8_string: Atom,
    targets: Atom,
    text: Atom,
}

impl Atoms {
    fn intern(conn: &RustConnection) -> Result<Self, String> {
        Ok(Self {
            clipboard: intern(conn, "CLIPBOARD")?,
            utf8_string: intern(conn, "UTF8_STRING")?,
            targets: intern(conn, "TARGETS")?,
            text: intern(conn, "TEXT")?,
        })
    }
}

/// The atom named `name`, which the X11 server makes when it is missing.
fn intern(conn: &RustConnection, name: &str) -> Result<Atom, String> {
    xproto::intern_atom(conn, false, name.as_bytes())
        .map_err(|error| format!("the X11 atom {name}: {error}"))?
        .reply()
        .map_err(|error| format!("the X11 atom {name}: {error}"))
        .map(|reply| reply.atom)
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    use super::*;
    use x11rb::COPY_DEPTH_FROM_PARENT;
    use x11rb::protocol::xproto::GetPropertyReply;

    /// How long a read waits for a backend to own the selection.
    const WAIT: Duration = Duration::from_secs(5);

    /// The length asked of a property read: a cap, not an allocation.
    const ANY_LENGTH: u32 = u32::MAX / 4;

    /// Waits before another read attempt. `std::thread::sleep` is banned here,
    /// so a channel that never sends holds the wait.
    fn wait_a_moment() {
        let (_sender, receiver) = mpsc::channel::<()>();
        let _ = receiver.recv_timeout(Duration::from_millis(20));
    }

    /// Stops a test on a desktop, where it would replace the owner's clipboard.
    fn require_test_display() {
        assert!(
            std::env::var_os("REAGENT_TEST_DISPLAY").is_some(),
            "these tests write the clipboard: set REAGENT_TEST_DISPLAY on a test display only"
        );
    }

    /// Holds the one CLIPBOARD of the display. Nextest runs each test in a
    /// process of its own, and two tests that take the selection at once would
    /// take it from each other.
    #[expect(
        clippy::disallowed_types,
        reason = "a lock file of a test, not the I/O of the agent"
    )]
    fn display_lock() -> std::fs::File {
        require_test_display();
        let file = std::fs::File::create(std::env::temp_dir().join("reagent-tui-clipboard.lock"))
            .expect("a lock file");
        file.lock().expect("the display lock");
        file
    }

    /// A connection, an unmapped window of the test's own, and the atoms.
    ///
    /// The connect is retried: a headless X server takes a moment to listen.
    fn client() -> (RustConnection, Window, Atoms) {
        let (conn, screen) = connect();
        let root = conn.setup().roots[screen].root;
        let window = conn.generate_id().expect("a window id");
        xproto::create_window(
            &conn,
            COPY_DEPTH_FROM_PARENT,
            window,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::new(),
        )
        .expect("a window");
        let atoms = Atoms::intern(&conn).expect("the atoms");
        (conn, window, atoms)
    }

    /// A connection to the display, for up to [`WAIT`].
    fn connect() -> (RustConnection, usize) {
        let start = Instant::now();
        loop {
            match x11rb::connect(None) {
                Ok(answer) => return answer,
                Err(error) => {
                    assert!(start.elapsed() < WAIT, "an X11 display: {error}");
                    wait_a_moment();
                }
            }
        }
    }

    /// Asks the CLIPBOARD owner for `target`, or nothing when no client owns it.
    fn read_selection(
        conn: &RustConnection,
        window: Window,
        atoms: &Atoms,
        target: Atom,
    ) -> Option<GetPropertyReply> {
        let property = intern(conn, "REAGENT_PASTE").expect("the property atom");
        xproto::convert_selection(
            conn,
            window,
            atoms.clipboard,
            target,
            property,
            x11rb::CURRENT_TIME,
        )
        .expect("a selection request");
        conn.flush().expect("a flush");
        loop {
            match conn.wait_for_event().expect("an event") {
                Event::SelectionNotify(event) if event.requestor == window => {
                    if event.property == x11rb::NONE {
                        return None;
                    }
                    return Some(
                        xproto::get_property(
                            conn,
                            true,
                            window,
                            property,
                            AtomEnum::ANY,
                            0,
                            ANY_LENGTH,
                        )
                        .expect("a property request")
                        .reply()
                        .expect("a property reply"),
                    );
                }
                _ => {}
            }
        }
    }

    /// Reads `target` until a client takes the selection, for up to [`WAIT`].
    fn paste_x11(
        conn: &RustConnection,
        window: Window,
        atoms: &Atoms,
        target: Atom,
    ) -> GetPropertyReply {
        let start = Instant::now();
        loop {
            if let Some(reply) = read_selection(conn, window, atoms, target) {
                return reply;
            }
            assert!(
                start.elapsed() < WAIT,
                "no client owned the X11 selection to answer"
            );
            wait_a_moment();
        }
    }

    /// Takes the CLIPBOARD selection with `window`.
    fn take(conn: &RustConnection, window: Window, atoms: &Atoms) {
        xproto::set_selection_owner(conn, window, atoms.clipboard, x11rb::CURRENT_TIME)
            .expect("a selection request")
            .check()
            .expect("the window takes the selection");
        conn.flush().expect("a flush");
    }

    /// Runs the X11 backend on a thread of its own and hands back its answer.
    fn serve(text: &str) -> (thread::JoinHandle<()>, mpsc::Receiver<Result<(), String>>) {
        let text = text.to_owned();
        let (sender, answers) = mpsc::channel();
        let backend = thread::spawn(move || {
            let _ = sender.send(x11(&text));
        });
        (backend, answers)
    }

    /// Reads the regular Wayland clipboard's text.
    fn paste_wayland() -> Result<Vec<u8>, String> {
        let (mut pipe, _mime) = wl_clipboard_rs::paste::get_contents(
            wl_clipboard_rs::paste::ClipboardType::Regular,
            wl_clipboard_rs::paste::Seat::Unspecified,
            wl_clipboard_rs::paste::MimeType::Text,
        )
        .map_err(|error| format!("the Wayland clipboard: {error}"))?;
        let mut bytes = Vec::new();
        pipe.read_to_end(&mut bytes)
            .map_err(|error| format!("the Wayland clipboard: {error}"))?;
        Ok(bytes)
    }

    /// Reads the Wayland clipboard until it holds `text`, for up to [`WAIT`].
    fn read_wayland(text: &str) -> Result<(), String> {
        let start = Instant::now();
        loop {
            let last = match paste_wayland() {
                Ok(bytes) if bytes == text.as_bytes() => return Ok(()),
                Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                Err(error) => error,
            };
            if start.elapsed() >= WAIT {
                return Err(format!("the Wayland clipboard held {last:?}"));
            }
            wait_a_moment();
        }
    }

    /// A tool the machine does not have holds no image.
    #[test]
    fn image_paste_of_a_missing_tool_is_no_image() {
        assert!(
            read("reagent-no-such-tool", &[]).is_none(),
            "a missing tool is no image"
        );
    }

    #[test]
    #[ignore = "needs a display"]
    fn x11_serves_the_text_as_utf8() {
        let _lock = display_lock();
        let text = "alpha bravo charlie \u{2603}";
        let (conn, window, atoms) = client();
        let (backend, answers) = serve(text);

        let reply = paste_x11(&conn, window, &atoms, atoms.utf8_string);

        assert_eq!(reply.type_, atoms.utf8_string);
        assert_eq!(reply.format, 8);
        assert_eq!(reply.value, text.as_bytes());
        take(&conn, window, &atoms);
        assert_eq!(answers.recv_timeout(WAIT), Ok(Ok(())));
        backend.join().expect("the backend thread");
    }

    #[test]
    #[ignore = "needs a display"]
    fn x11_answers_the_targets() {
        let _lock = display_lock();
        let text = "alpha bravo";
        let (conn, window, atoms) = client();
        let (backend, answers) = serve(text);

        let reply = paste_x11(&conn, window, &atoms, atoms.targets);

        assert_eq!(reply.format, 32);
        assert!(
            reply
                .value32()
                .expect("atoms")
                .any(|atom| atom == atoms.utf8_string)
        );
        take(&conn, window, &atoms);
        assert_eq!(answers.recv_timeout(WAIT), Ok(Ok(())));
        backend.join().expect("the backend thread");
    }

    #[test]
    #[ignore = "needs a display"]
    fn x11_refuses_a_text_larger_than_a_request() {
        let _lock = display_lock();
        let (conn, window, atoms) = client();
        let text = "x".repeat(64 * 1024 * 1024);

        let error = x11(&text).expect_err("the text is over one X11 request");

        assert!(error.contains("at most"), "{error}");
        // The refusal came before the selection was taken, so a new window takes
        // it at once.
        take(&conn, window, &atoms);
        let owner = xproto::get_selection_owner(&conn, atoms.clipboard)
            .expect("a selection request")
            .reply()
            .expect("a selection reply");
        assert_eq!(owner.owner, window);
    }

    #[test]
    #[ignore = "needs a display"]
    fn wayland_serves_the_text() {
        require_test_display();
        let text = "alpha bravo charlie \u{2603}";
        let owned = text.to_owned();
        // The call serves until the compositor drops the source, so the thread
        // is left to the process exit.
        thread::spawn(move || {
            let _ = wayland(&owned);
        });

        read_wayland(text).expect("the Wayland clipboard takes the text");
    }
}
