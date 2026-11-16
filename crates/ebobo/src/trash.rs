//! Getting rid of a game's file: into the system Trash where that works, else for good.
//! Owner: trash slice.

use std::fs;
use std::path::Path;

/// What happens to a file that is disposed of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disposal {
    /// It moves to the system Trash and can be put back from there.
    Trash,
    /// It is deleted for good.
    Permanent,
}

/// What `dispose` would do with `path` now, so the question can say it before asking.
pub fn plan(path: &Path) -> Disposal {
    let _ = path;
    Disposal::Permanent
}

/// Removes the file as `plan` said and returns what was done. On an error the file is still
/// where it was; the message is for the user.
pub fn dispose(path: &Path) -> Result<Disposal, String> {
    fs::remove_file(path).map(|()| Disposal::Permanent).map_err(|e| e.to_string())
}
