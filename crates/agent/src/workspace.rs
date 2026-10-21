//! The working folder, as the loop sees it.

use futures_util::future::BoxFuture;

/// A view of the working folder's file state.
pub trait Workspace: Send + Sync {
    /// A hash of the folder's file state that differs when a file was created,
    /// deleted or changed, or `None` when the state cannot be read.
    fn fingerprint(&self) -> BoxFuture<'_, Option<u64>>;
}
