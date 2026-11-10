//! One folder of the file system, watched for the entries that come and go.
//!
//! The app holds it open on the discovery folder of the live workers, so a
//! worker that starts, stops or waits for an approval tells a TUI that sits
//! idle the moment it writes its entry. The operating system holds the
//! descriptor and the runtime waits on it, so there is no thread, no timer and
//! no wake-up while the folder does not change.

use std::io;
use std::path::Path;

/// A folder whose entries are announced, one change at a time.
#[derive(Debug)]
pub struct Watch {
    /// The descriptor the runtime waits on: the folder added to a kqueue on
    /// macOS and to an inotify on Linux.
    #[cfg(target_os = "macos")]
    fd: tokio::io::unix::AsyncFd<kqueue::Watcher>,
    #[cfg(target_os = "linux")]
    fd: tokio::io::unix::AsyncFd<inotify::Inotify>,
}

impl Watch {
    /// Watches `folder` for an entry created in it, renamed into it or removed
    /// from it.
    ///
    /// A folder that is not there and a platform with neither of the two
    /// notifications are errors of this call: the caller then reads the folder
    /// when it has another reason to and no notification wakes it.
    #[cfg(target_os = "macos")]
    pub fn new(folder: &Path) -> io::Result<Watch> {
        let mut watcher = kqueue::Watcher::new()?;
        // The vnode of a folder reports a write for every entry that lands in
        // it or leaves it, the rename of an entry included.
        watcher.add_filename(
            folder,
            kqueue::EventFilter::EVFILT_VNODE,
            kqueue::FilterFlag::NOTE_WRITE,
        )?;
        watcher.watch()?;
        Ok(Watch {
            // The folder is read and never written, and a kqueue answers a
            // write interest with an invalid argument.
            #[expect(
                deprecated,
                reason = "the watch owns the descriptor for as long as it is registered"
            )]
            fd: tokio::io::unix::AsyncFd::with_interest(watcher, tokio::io::Interest::READABLE)?,
        })
    }

    /// Watches `folder` for an entry created in it, renamed into it or removed
    /// from it.
    ///
    /// A folder that is not there and a platform with neither of the two
    /// notifications are errors of this call: the caller then reads the folder
    /// when it has another reason to and no notification wakes it.
    #[cfg(target_os = "linux")]
    pub fn new(folder: &Path) -> io::Result<Watch> {
        let inotify = inotify::Inotify::init()?;
        inotify.watches().add(
            folder,
            inotify::WatchMask::CREATE
                | inotify::WatchMask::MOVED_TO
                | inotify::WatchMask::MOVED_FROM
                | inotify::WatchMask::DELETE,
        )?;
        Ok(Watch {
            #[expect(
                deprecated,
                reason = "the watch owns the descriptor for as long as it is registered"
            )]
            fd: tokio::io::unix::AsyncFd::with_interest(inotify, tokio::io::Interest::READABLE)?,
        })
    }

    /// Refuses, since neither a kqueue nor an inotify is there to hold the
    /// folder.
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    pub fn new(folder: &Path) -> io::Result<Watch> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("watching {} needs a kqueue or an inotify", folder.display()),
        ))
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl Watch {
    /// Resolves once the folder changed, with every event it holds taken.
    ///
    /// The wait arms nothing: a change that arrives while this runs is one
    /// call more, and a folder that does not change wakes this not at all.
    pub async fn changed(&mut self) -> io::Result<()> {
        loop {
            // The readiness goes before the events are taken, so the wait that
            // follows asks the descriptor itself rather than a stale flag.
            self.fd.readable().await?.clear_ready();
            if self.drain()? {
                return Ok(());
            }
        }
    }

    /// Takes every event the descriptor holds, and says whether it held one.
    ///
    /// An event the kernel could not queue counts too: it says the folder
    /// changed, which is all this watch promised.
    #[cfg(target_os = "macos")]
    fn drain(&mut self) -> io::Result<bool> {
        let watcher = self.fd.get_ref();
        let mut seen = false;
        while watcher.poll(Some(std::time::Duration::ZERO)).is_some() {
            seen = true;
        }
        Ok(seen)
    }

    /// Takes every event the descriptor holds, and says whether it held one.
    #[cfg(target_os = "linux")]
    fn drain(&mut self) -> io::Result<bool> {
        let mut buffer = [0u8; EVENT_BUFFER];
        let mut seen = false;
        loop {
            match self.fd.get_mut().read_events(&mut buffer) {
                Ok(events) => seen |= events.count() > 0,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(seen),
                Err(error) => return Err(error),
            }
        }
    }
}

/// A watch the platform refused, which is never built and so never waits.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
impl Watch {
    /// Never resolves, since [`Watch::new`] never hands out a watch here.
    pub async fn changed(&mut self) -> io::Result<()> {
        std::future::pending().await
    }
}

/// The bytes of the inotify events one read takes.
#[cfg(target_os = "linux")]
const EVENT_BUFFER: usize = 1024;

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod tests {
    use std::time::Duration;

    use super::*;

    /// How long a test waits for the change it made.
    const SOON: Duration = Duration::from_secs(1);

    /// How long a test looks at a folder that does not change.
    const QUIET: Duration = Duration::from_millis(300);

    /// A folder of its own, removed with the test.
    fn folder() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("reagent-watch")
            .tempdir()
            .expect("a temp folder")
    }

    #[tokio::test]
    async fn an_entry_written_under_a_temp_name_and_renamed_in_resolves() {
        let folder = folder();
        let mut watch = Watch::new(folder.path()).expect("a watch");
        let temp = folder.path().join("s1.json.tmp-1");
        tokio::fs::write(&temp, b"{}")
            .await
            .expect("the entry under its temp name");
        tokio::fs::rename(&temp, folder.path().join("s1.json"))
            .await
            .expect("the rename of the entry");
        tokio::time::timeout(SOON, watch.changed())
            .await
            .expect("a change in time")
            .expect("the watch answers");
    }

    #[tokio::test]
    async fn a_removed_entry_resolves() {
        let folder = folder();
        let entry = folder.path().join("s1.json");
        tokio::fs::write(&entry, b"{}").await.expect("the entry");
        let mut watch = Watch::new(folder.path()).expect("a watch");
        tokio::fs::remove_file(&entry)
            .await
            .expect("the removal of the entry");
        tokio::time::timeout(SOON, watch.changed())
            .await
            .expect("a change in time")
            .expect("the watch answers");
    }

    #[tokio::test]
    async fn a_folder_that_does_not_change_does_not_resolve() {
        let folder = folder();
        tokio::fs::write(folder.path().join("s1.json"), b"{}")
            .await
            .expect("the entry");
        let mut watch = Watch::new(folder.path()).expect("a watch");
        let quiet = tokio::time::timeout(QUIET, watch.changed()).await;
        assert!(quiet.is_err(), "the watch resolved with nothing changed");
    }
}
