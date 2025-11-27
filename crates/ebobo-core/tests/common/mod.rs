//! Shared by the integration tests that run the fetched test ROMs.

use std::path::PathBuf;

/// `tests/roms`, or `None` after a one-line notice when scripts/fetch-test-roms.sh was not run.
pub fn roms() -> Option<PathBuf> {
    let dir = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/roms"));
    if dir.is_dir() {
        Some(dir)
    } else {
        println!("test ROMs missing: run scripts/fetch-test-roms.sh");
        None
    }
}
