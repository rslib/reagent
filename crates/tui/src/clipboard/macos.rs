//! The macOS clipboard, through the general `NSPasteboard`.
//!
//! AppKit adds about 2.7 MB of RSS to a process that links it, and most runs
//! never copy. So it loads at the first copy, and no crate here links it. That
//! leaves no bindings for `NSPasteboard`, which is why this module has `unsafe`.

#![allow(unsafe_code)]

use std::ffi::CStr;

use objc2::msg_send;
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::{AnyClass, AnyObject};
use objc2_foundation::NSString;

use super::ClipImage;

const APPKIT: &CStr = c"/System/Library/Frameworks/AppKit.framework/AppKit";

/// The pasteboard type of plain UTF-8 text.
const UTF8_TEXT: &str = "public.utf8-plain-text";

/// The pasteboard type of a PNG image.
const PNG_TYPE: &str = "public.png";

/// The pasteboard type of a TIFF image, which macOS also holds a copy of.
const TIFF_TYPE: &str = "public.tiff";

/// How many times a write to the pasteboard is tried before it gives up.
const WRITE_TRIES: u32 = 5;

/// The wait between two tries of a write.
const WRITE_WAIT: std::time::Duration = std::time::Duration::from_millis(50);

/// Puts `text` in the general pasteboard.
pub fn write(text: &str) -> Result<(), String> {
    autoreleasepool(|_| {
        let class = pasteboard_class()?;
        // SAFETY: `+generalPasteboard` takes no argument and returns an object.
        let board: Option<Retained<AnyObject>> = unsafe { msg_send![class, generalPasteboard] };
        let board = board.ok_or("no general pasteboard")?;
        write_to(&board, text)
    })
}

/// Reads the image of the general pasteboard: its PNG, else its TIFF.
pub fn read_image() -> Result<Option<ClipImage>, String> {
    autoreleasepool(|_| {
        let class = pasteboard_class()?;
        // SAFETY: `+generalPasteboard` takes no argument and returns an object.
        let board: Option<Retained<AnyObject>> = unsafe { msg_send![class, generalPasteboard] };
        let board = board.ok_or("no general pasteboard")?;
        read_from(&board)
    })
}

/// The image `board` holds, as its PNG or else its TIFF: no image is
/// `Ok(None)`.
fn read_from(board: &AnyObject) -> Result<Option<ClipImage>, String> {
    for (kind, ext) in [(PNG_TYPE, "png"), (TIFF_TYPE, "tiff")] {
        let kind = NSString::from_str(kind);
        // SAFETY: `board` is an `NSPasteboard` and `-dataForType:` takes a type
        // string and returns the data it holds, or nil.
        let data: Option<Retained<AnyObject>> = unsafe { msg_send![board, dataForType: &*kind] };
        let Some(data) = data else {
            continue;
        };
        // SAFETY: `-bytes` returns the buffer of the data and `-length` its
        // length; the copy is made while the data is alive.
        let bytes = unsafe {
            let bytes: *const std::ffi::c_void = msg_send![&*data, bytes];
            let length: usize = msg_send![&*data, length];
            if bytes.is_null() {
                Vec::new()
            } else {
                std::slice::from_raw_parts(bytes.cast::<u8>(), length).to_vec()
            }
        };
        if let Some(image) = crate::clipboard::capped(bytes, ext)? {
            return Ok(Some(image));
        }
    }
    Ok(None)
}

/// Loads AppKit and finds `NSPasteboard`.
///
/// The handle is never closed, because the class must stay loaded. A repeat call
/// only counts up the same handle.
fn pasteboard_class() -> Result<&'static AnyClass, String> {
    // SAFETY: `APPKIT` is a NUL-terminated string that lives for the program.
    let handle = unsafe { libc::dlopen(APPKIT.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL) };
    if handle.is_null() {
        return Err("AppKit did not load".to_owned());
    }
    AnyClass::get(c"NSPasteboard").ok_or_else(|| "AppKit has no NSPasteboard".to_owned())
}

/// Clears `board` and gives it `text` as its one item.
///
/// The type comes from its name as a string, not from the framework's `static`.
fn write_to(board: &AnyObject, text: &str) -> Result<(), String> {
    let value = NSString::from_str(text);
    let kind = NSString::from_str(UTF8_TEXT);
    let written = with_tries(|| {
        // SAFETY: `board` is an `NSPasteboard`. `-clearContents` returns
        // `NSInteger`, and `-setString:forType:` takes two strings and returns
        // `BOOL`.
        unsafe {
            let _: isize = msg_send![board, clearContents];
            msg_send![board, setString: &*value, forType: &*kind]
        }
    });
    if written {
        Ok(())
    } else {
        Err("the pasteboard refused the text".to_owned())
    }
}

/// Runs `attempt` until it returns true, at most [`WRITE_TRIES`] times.
///
/// macOS refuses a write now and then and gives no reason, and without this the
/// copy is lost with no word. The caller is the clipboard's own thread, so the
/// wait holds no frame.
#[expect(
    clippy::disallowed_methods,
    reason = "runs on the clipboard's own thread"
)]
fn with_tries(mut attempt: impl FnMut() -> bool) -> bool {
    for try_number in 0..WRITE_TRIES {
        if try_number > 0 {
            std::thread::sleep(WRITE_WAIT);
        }
        if attempt() {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The eight bytes every PNG file starts with: a read hands them back as
    /// they are.
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";

    /// A board of the test's own, so the owner's clipboard stays as it is.
    fn private(class: &AnyClass) -> Retained<AnyObject> {
        // SAFETY: `+pasteboardWithUniqueName` takes no argument and returns an
        // object.
        let board: Option<Retained<AnyObject>> =
            unsafe { msg_send![class, pasteboardWithUniqueName] };
        board.expect("a pasteboard")
    }

    /// Gives `board` the bytes of [`PNG`] as its image.
    fn put_png(board: &AnyObject) {
        let class = AnyClass::get(c"NSData").expect("Foundation has NSData");
        let kind = NSString::from_str(PNG_TYPE);
        let written = with_tries(|| {
            // SAFETY: `+dataWithBytes:length:` copies `PNG.len()` bytes of `PNG`
            // into a data object, `-clearContents` returns `NSInteger`, and
            // `-setData:forType:` takes the data and the type string and returns
            // `BOOL`.
            unsafe {
                let data: Option<Retained<AnyObject>> = msg_send![class, dataWithBytes: PNG.as_ptr().cast::<std::ffi::c_void>(), length: PNG.len()];
                let data = data.expect("the data");
                let _: isize = msg_send![board, clearContents];
                msg_send![board, setData: &*data, forType: &*kind]
            }
        });
        assert!(written, "the pasteboard takes the image");
    }

    /// A write the board refuses at first goes through on a later try.
    #[test]
    fn a_refused_write_is_tried_again() {
        let mut tries = 0;
        let written = with_tries(|| {
            tries += 1;
            tries == WRITE_TRIES
        });
        assert!(written, "the last try was taken");
        assert_eq!(tries, WRITE_TRIES, "and each earlier try ran");
    }

    /// A board that always refuses ends the tries with a failure.
    #[test]
    fn a_write_refused_every_time_gives_up() {
        let mut tries = 0;
        let written = with_tries(|| {
            tries += 1;
            false
        });
        assert!(!written, "the failure reaches the caller");
        assert_eq!(tries, WRITE_TRIES, "after the fixed number of tries");
    }

    /// Gives `board` back: it is the test's own, and the process outlives it.
    fn release(board: &Retained<AnyObject>) {
        // SAFETY: `-releaseGlobally` takes no argument and returns nothing.
        unsafe {
            let () = msg_send![&**board, releaseGlobally];
        }
    }

    /// The PNG of a board of the test's own reads back with its extension.
    #[test]
    fn image_paste_reads_the_png_of_a_private_pasteboard() {
        let class = pasteboard_class().expect("the class");
        autoreleasepool(|_| {
            let board = private(class);
            put_png(&board);
            let image = read_from(&board).expect("the read").expect("an image");
            assert_eq!(image.ext, "png", "the type names the extension");
            assert_eq!(image.bytes, PNG, "and the bytes come back as they went");
            release(&board);
        });
    }

    /// A board with text alone holds no image.
    #[test]
    fn image_paste_finds_no_image_on_a_text_pasteboard() {
        let class = pasteboard_class().expect("the class");
        autoreleasepool(|_| {
            let board = private(class);
            write_to(&board, "alpha bravo").expect("the pasteboard takes the text");
            assert!(
                read_from(&board).expect("the read").is_none(),
                "text is no image"
            );
            release(&board);
        });
    }

    fn appkit_is_loaded() -> bool {
        // SAFETY: `APPKIT` is a NUL-terminated string. `RTLD_NOLOAD` loads nothing.
        let handle = unsafe { libc::dlopen(APPKIT.as_ptr(), libc::RTLD_LAZY | libc::RTLD_NOLOAD) };
        !handle.is_null()
    }

    /// The only test that loads AppKit, as the first check needs a process that
    /// has not loaded it. The pasteboard has a unique name: the general one is
    /// the owner's, and no test writes it.
    #[test]
    fn appkit_loads_at_the_first_copy_and_the_text_reads_back() {
        assert!(
            !appkit_is_loaded(),
            "AppKit is in the process before a copy"
        );
        let class = pasteboard_class().expect("the class");
        assert!(appkit_is_loaded());

        autoreleasepool(|_| {
            // SAFETY: `+pasteboardWithUniqueName` takes no argument and returns an
            // object.
            let board: Option<Retained<AnyObject>> =
                unsafe { msg_send![class, pasteboardWithUniqueName] };
            let board = board.expect("a pasteboard");

            write_to(&board, "alpha bravo").expect("the pasteboard takes the text");

            let kind = NSString::from_str(UTF8_TEXT);
            // SAFETY: `-stringForType:` takes a string and returns a string or nil.
            // `-releaseGlobally` takes no argument and returns nothing.
            let read: Option<Retained<NSString>> = unsafe {
                let read = msg_send![&*board, stringForType: &*kind];
                let () = msg_send![&*board, releaseGlobally];
                read
            };
            assert_eq!(read.expect("the text").to_string(), "alpha bravo");
        });
    }
}
