//! Downloading through the system `curl`, as the updater does. Owner: image slice.

use std::path::Path;

/// Why a download gave nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FetchError {
    /// The server has no such file (HTTP 404): do not ask again.
    NotFound,
    /// Anything else (offline, timeout, another status): worth another try later.
    Failed(String),
}

/// Downloads `url` into the file `to` (written under a temporary name and renamed, so a
/// partial file never appears under `to`). Follows redirects, gives up after `timeout_secs`.
pub fn fetch(_url: &str, _to: &Path, _timeout_secs: u32) -> Result<(), FetchError> {
    todo!("owner: image slice")
}
