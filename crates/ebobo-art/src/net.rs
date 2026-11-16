//! Downloading through the system `curl`, as the updater does. Owner: image slice.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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
///
/// The temporary file is `to` with `.part` appended; it is gone when this returns, whatever
/// the outcome. The parent folder of `to` is created. Blocks until `curl` exits. A `file://`
/// URL works too (curl reports status 000 for it).
pub fn fetch(url: &str, to: &Path, timeout_secs: u32) -> Result<(), FetchError> {
    if let Some(parent) = to.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| FetchError::Failed(format!("create folder: {e}")))?;
    }
    let mut part: OsString = to.as_os_str().to_owned();
    part.push(".part");
    let part = PathBuf::from(part);

    let result = download(url, &part, timeout_secs).and_then(|()| std::fs::rename(&part, to).map_err(|e| FetchError::Failed(format!("rename: {e}"))));
    if result.is_err() {
        let _ = std::fs::remove_file(&part);
    }
    result
}

/// Windows opens a console window for every console program a windowed program starts; this
/// flag (CREATE_NO_WINDOW) keeps it hidden. Elsewhere there is nothing to hide.
fn quiet(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    cmd
}

/// Runs `curl` writing the body to `part`. `Ok` when it exited with 0 and the status is 200
/// (or 000: a `file://` URL has no status).
fn download(url: &str, part: &Path, timeout_secs: u32) -> Result<(), FetchError> {
    let output = quiet(&mut Command::new("curl"))
        .args(["-sSL", "--max-time", &timeout_secs.to_string(), "-o"])
        .arg(part)
        // `--url` so that a URL starting with a dash is never taken for an option.
        .args(["-w", "%{http_code}", "--url", url])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| FetchError::Failed(format!("curl: {e}")))?;
    let status = String::from_utf8_lossy(&output.stdout);
    let status = status.trim();
    if status == "404" {
        return Err(FetchError::NotFound);
    }
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let reason: String = stderr.lines().next().unwrap_or("").trim().chars().take(200).collect();
        return Err(FetchError::Failed(match (reason.is_empty(), output.status.code()) {
            (false, _) => reason,
            (true, Some(code)) => format!("curl exit {code}"),
            (true, None) => "curl killed".into(),
        }));
    }
    match status {
        "200" | "000" => Ok(()),
        other => Err(FetchError::Failed(format!("HTTP {other}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ebobo-art-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn part_of(to: &Path) -> PathBuf {
        let mut part = to.as_os_str().to_owned();
        part.push(".part");
        PathBuf::from(part)
    }

    #[test]
    fn copies_a_file_url() {
        let dir = temp_dir("copy");
        let source = dir.join("source.bin");
        std::fs::write(&source, b"box art bytes").unwrap();
        // The parent folder does not exist yet.
        let to = dir.join("cache").join("deep").join("copy.bin");
        assert_eq!(fetch(&format!("file://{}", source.display()), &to, 10), Ok(()));
        assert_eq!(std::fs::read(&to).unwrap(), b"box art bytes");
        assert!(!part_of(&to).exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_missing_file_url_fails_and_leaves_nothing() {
        let dir = temp_dir("missing");
        let to = dir.join("copy.bin");
        let result = fetch(&format!("file://{}", dir.join("no-such-file").display()), &to, 10);
        assert!(matches!(&result, Err(FetchError::Failed(reason)) if !reason.is_empty()), "{result:?}");
        assert!(!to.exists());
        assert!(!part_of(&to).exists());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
