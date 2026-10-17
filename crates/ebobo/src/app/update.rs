//! Self-update from the GitHub releases of idoo/ebobo, through the system `curl`.
//!
//! A background thread checks the latest tag, downloads the asset for this platform, checks it
//! against the release's `SHA256SUMS`, unpacks it next to the running copy (same volume) and, on
//! macOS, checks the code signature. The app applies the prepared update only from the main menu
//! (never during a game): it swaps the files in place and relaunches with the same arguments.

use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::thread;

const REPO: &str = "https://github.com/idoo/ebobo/releases";
/// Developer ID team of the signed Mac builds.
const MAC_REQUIREMENT: &str = "-R=anchor apple generic and certificate leaf[subject.OU] = \"6NE65QKPJ6\"";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u32, pub u32, pub u32);

impl Version {
    /// "0.2.3" or "v0.2.3" (a pre-release/build suffix after `-`/`+` is ignored).
    pub fn parse(s: &str) -> Option<Version> {
        let s = s.trim();
        let s = s.strip_prefix('v').unwrap_or(s);
        let core = s.split(['-', '+']).next()?;
        let mut parts = core.split('.').map(|p| p.parse::<u32>().ok());
        let v = Version(parts.next()??, parts.next()??, parts.next()??);
        parts.next().is_none().then_some(v)
    }

    fn tag(self) -> String {
        format!("v{self}")
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// The running version; `EBOBO_FAKE_VERSION` overrides it (testing).
pub fn running_version() -> Version {
    std::env::var("EBOBO_FAKE_VERSION")
        .ok()
        .and_then(|v| Version::parse(&v))
        .or_else(|| Version::parse(env!("CARGO_PKG_VERSION")))
        .expect("CARGO_PKG_VERSION is semver")
}

/// Version from the `location: …/releases/tag/vX.Y.Z` header of `curl -sI …/releases/latest`.
pub fn parse_location(headers: &str) -> Option<Version> {
    headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if !name.trim().eq_ignore_ascii_case("location") {
            return None;
        }
        let value = value.trim();
        let tag = &value[value.rfind("/tag/")? + 5..];
        Version::parse(tag.trim_end_matches('/'))
    })
}

/// Lowercase hex digest for `name` in a `shasum -a 256` listing (`<hex>  <name>` or
/// `<hex> *<name>`).
pub fn expected_sha(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (hex, file) = line.trim().split_once(char::is_whitespace)?;
        let file = file.trim_start().trim_start_matches('*');
        let file = file.rsplit('/').next().unwrap_or(file);
        (file == name && hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| hex.to_ascii_lowercase())
    })
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Checks `path` against its entry for `name` in `sums`.
pub fn check_sha(sums: &str, name: &str, path: &Path) -> Result<(), String> {
    let want = expected_sha(sums, name).ok_or_else(|| format!("{name} is not listed in SHA256SUMS"))?;
    let got = sha256_file(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if got == want {
        Ok(())
    } else {
        Err(format!("{name}: SHA-256 mismatch (expected {want}, got {got})"))
    }
}

/// What gets replaced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// `<bundle>.app` (running from `<bundle>.app/Contents/MacOS/ebobo`).
    Mac { bundle: PathBuf },
    /// The running executable (Steam Deck install).
    Linux { exe: PathBuf },
}

impl Target {
    fn detect() -> Option<Target> {
        let exe = std::env::current_exe().ok()?;
        target_for(&exe)
    }

    /// Directory the new copy is unpacked in and swapped from.
    fn dir(&self) -> &Path {
        match self {
            Target::Mac { bundle } => bundle.parent().unwrap_or(Path::new("/")),
            Target::Linux { exe } => exe.parent().unwrap_or(Path::new("/")),
        }
    }

    fn asset(&self) -> &'static str {
        match self {
            Target::Mac { .. } => "ebobo-mac.zip",
            Target::Linux { .. } => "ebobo-steamdeck.tar.gz",
        }
    }
}

fn target_for(exe: &Path) -> Option<Target> {
    if cfg!(target_os = "macos") {
        let macos = exe.parent()?;
        let contents = macos.parent()?;
        let bundle = contents.parent()?;
        let is_bundle = macos.file_name()? == "MacOS"
            && contents.file_name()? == "Contents"
            && bundle.extension().is_some_and(|e| e == "app");
        is_bundle.then(|| Target::Mac { bundle: bundle.to_path_buf() })
    } else if cfg!(target_os = "linux") {
        Some(Target::Linux { exe: exe.to_path_buf() })
    } else {
        None
    }
}

/// Why an update cannot be installed in place (shown once per session).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Blocker {
    /// macOS: translocated or read-only location; move ebobo.app to Applications.
    MoveToApplications,
    /// Linux: install dir not writable; re-run the installer.
    RerunInstaller,
}

fn blocker(target: &Target) -> Option<Blocker> {
    let ok = writable(target.dir());
    match target {
        Target::Mac { bundle } if !ok || bundle.to_string_lossy().contains("/AppTranslocation/") => {
            Some(Blocker::MoveToApplications)
        }
        Target::Linux { .. } if !ok => Some(Blocker::RerunInstaller),
        _ => None,
    }
}

fn writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".ebobo-write-test-{}", std::process::id()));
    let ok = std::fs::File::create(&probe).is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

/// A downloaded, verified, unpacked update waiting to be swapped in.
#[derive(Debug)]
pub struct Prepared {
    pub version: Version,
    target: Target,
    /// New `.app` bundle or `ebobo` binary inside `staging`.
    new_path: PathBuf,
    staging: PathBuf,
}

enum Event {
    Newer(Version),
    Ready(Prepared),
    Blocked(Blocker),
    Done,
}

#[derive(Debug)]
enum Status {
    Idle,
    Working(Option<Version>),
    Ready(Prepared),
    /// Nothing more this session (up to date, failed, blocked or applied).
    Finished,
}

pub struct Updater {
    rx: Option<mpsc::Receiver<Event>>,
    status: Status,
    blocker_shown: bool,
    /// `updating_to` changed since `take_changed`.
    changed: bool,
}

impl Updater {
    pub fn new() -> Updater {
        Updater { rx: None, status: Status::Idle, blocker_shown: false, changed: false }
    }

    /// Starts a background check+download unless one already ran or runs this session.
    /// `force` (netplay version mismatch) also re-checks after an "up to date" result.
    pub fn start(&mut self, force: bool) {
        match self.status {
            Status::Idle => {}
            Status::Finished if force && self.rx.is_none() => {}
            _ => return,
        }
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        self.status = Status::Working(None);
        let spawned = thread::Builder::new().name("ebobo-update".into()).spawn(move || {
            let event = match prepare(&tx) {
                Ok(Some(prepared)) => Event::Ready(prepared),
                Ok(None) => Event::Done,
                Err(err) => {
                    eprintln!("ebobo: update: {err}");
                    Event::Done
                }
            };
            let _ = tx.send(event);
        });
        if spawned.is_err() {
            self.rx = None;
            self.status = Status::Finished;
        }
    }

    /// Drains worker events; returns a blocker to show (at most once per session).
    pub fn poll(&mut self) -> Option<Blocker> {
        let rx = self.rx.take()?;
        let mut show = None;
        while let Ok(event) = rx.try_recv() {
            if self.handle(event, &mut show) {
                return show;
            }
        }
        self.rx = Some(rx);
        show
    }

    /// Applies one worker event; `true` when the worker is finished.
    fn handle(&mut self, event: Event, show: &mut Option<Blocker>) -> bool {
        let done = match event {
            Event::Newer(v) => {
                self.status = Status::Working(Some(v));
                false
            }
            Event::Ready(p) => {
                self.status = Status::Ready(p);
                true
            }
            Event::Blocked(b) => {
                if !self.blocker_shown {
                    self.blocker_shown = true;
                    *show = Some(b);
                }
                false
            }
            Event::Done => {
                self.status = Status::Finished;
                true
            }
        };
        self.changed = true;
        if done {
            self.rx = None;
        }
        done
    }

    /// Whether the status line needs redrawing (reset by this call).
    pub fn take_changed(&mut self) -> bool {
        std::mem::take(&mut self.changed)
    }

    /// Version being installed (for the main-menu status line).
    pub fn updating_to(&self) -> Option<Version> {
        match &self.status {
            Status::Working(v) => *v,
            Status::Ready(p) => Some(p.version),
            _ => None,
        }
    }

    pub fn is_ready(&self) -> bool {
        matches!(self.status, Status::Ready(_))
    }

    /// On quit: lets a running download finish, then installs it without relaunching.
    pub fn install_on_quit(&mut self) {
        if let (Status::Working(Some(v)), Some(rx)) = (&self.status, self.rx.take()) {
            eprintln!("ebobo: update: finishing the v{v} download before quitting");
            let mut show = None;
            while let Ok(event) = rx.recv() {
                if self.handle(event, &mut show) {
                    break;
                }
            }
        }
        self.apply(&[], false);
    }

    /// Swaps the prepared update in; with `relaunch` starts it with `args` and exits. Returns
    /// when not relaunching or on failure (logged; the running copy keeps working).
    pub fn apply(&mut self, args: &[String], relaunch: bool) {
        let Status::Ready(p) = std::mem::replace(&mut self.status, Status::Finished) else { return };
        self.changed = true;
        eprintln!("ebobo: update: installing v{}", p.version);
        let swapped = match &p.target {
            Target::Mac { bundle } => swap_mac(bundle, &p.new_path, verify_mac),
            Target::Linux { exe } => swap_linux(exe, &p.new_path),
        };
        let _ = std::fs::remove_dir_all(&p.staging);
        let result = swapped.and_then(|()| {
            let marker = marker_path();
            let _ = marker.parent().map(std::fs::create_dir_all);
            let _ = std::fs::write(&marker, p.version.to_string());
            Ok(())
        });
        let result = result.and_then(|()| match (&p.target, relaunch) {
            (_, false) => Ok(()),
            (Target::Mac { bundle }, true) => {
                let status = Command::new("open").arg("-n").arg(bundle).arg("--args").args(args).status();
                match status {
                    Ok(s) if s.success() => std::process::exit(0),
                    Ok(s) => Err(format!("open: {s}")),
                    Err(e) => Err(format!("open: {e}")),
                }
            }
            (Target::Linux { exe }, true) => exec(exe, args),
        });
        match result {
            Ok(()) => eprintln!("ebobo: update: v{} installed", p.version),
            Err(err) => eprintln!("ebobo: update: {err}"),
        }
    }
}

#[cfg(unix)]
fn exec(exe: &Path, args: &[String]) -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    Err(format!("exec {}: {}", exe.display(), Command::new(exe).args(args).exec()))
}

#[cfg(not(unix))]
fn exec(_exe: &Path, _args: &[String]) -> Result<(), String> {
    Err("relaunch is not supported on this platform".into())
}

/// Holds the version of the last update installed (see `prepare`).
fn marker_path() -> PathBuf {
    crate::config::data_dir().join("last-update")
}

fn run(cmd: &mut Command) -> Result<String, String> {
    let out = cmd.output().map_err(|e| format!("{cmd:?}: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!("{cmd:?}: {} {}", out.status, String::from_utf8_lossy(&out.stderr).trim()))
    }
}

fn latest_version() -> Result<Version, String> {
    let headers = run(Command::new("curl").args(["-fsSI", "--max-time", "20", &format!("{REPO}/latest")]))?;
    parse_location(&headers).ok_or_else(|| "no release tag in the Location header".to_string())
}

fn download(url: &str, to: &Path) -> Result<(), String> {
    run(Command::new("curl").args(["-fsSL", "--max-time", "600", "-o"]).arg(to).arg(url)).map(drop)
}

/// Worker: check, download, verify, unpack. `Ok(None)`: nothing to install.
fn prepare(tx: &mpsc::Sender<Event>) -> Result<Option<Prepared>, String> {
    let running = running_version();
    let latest = latest_version()?;
    if latest <= running {
        eprintln!("ebobo: update: v{running} is current (latest v{latest})");
        return Ok(None);
    }
    eprintln!("ebobo: update: v{latest} is available (running v{running})");
    // A release whose build still reports an older version would be installed and relaunched
    // forever: never install the same version twice.
    let last = std::fs::read_to_string(marker_path()).ok().and_then(|s| Version::parse(s.trim()));
    if last == Some(latest) {
        eprintln!("ebobo: update: v{latest} is installed already but reports v{running}; not installing it again");
        return Ok(None);
    }
    let Some(target) = Target::detect() else {
        eprintln!("ebobo: update: not running from an installed app; skipping");
        return Ok(None);
    };
    if let Some(b) = blocker(&target) {
        eprintln!("ebobo: update: cannot replace {} ({b:?})", target.dir().display());
        let _ = tx.send(Event::Blocked(b));
        return Ok(None);
    }
    let _ = tx.send(Event::Newer(latest));
    let staging = target.dir().join(format!(".ebobo-update-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|e| format!("{}: {e}", staging.display()))?;
    let result = fetch_and_unpack(&target, latest, &staging, verify_mac);
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    result.map(|new_path| Some(Prepared { version: latest, target, new_path, staging }))
}

fn fetch_and_unpack(
    target: &Target,
    version: Version,
    staging: &Path,
    verify: fn(&Path) -> Result<(), String>,
) -> Result<PathBuf, String> {
    let base = format!("{REPO}/download/{}", version.tag());
    let sums_path = staging.join("SHA256SUMS");
    download(&format!("{base}/SHA256SUMS"), &sums_path)
        .map_err(|e| format!("v{version} has no SHA256SUMS, not installing ({e})"))?;
    let sums = std::fs::read_to_string(&sums_path).map_err(|e| format!("SHA256SUMS: {e}"))?;
    let asset = target.asset();
    let archive = staging.join(asset);
    download(&format!("{base}/{asset}"), &archive)?;
    check_sha(&sums, asset, &archive)?;
    eprintln!("ebobo: update: {asset} checksum ok");
    let out = staging.join("new");
    std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    match target {
        Target::Mac { .. } => {
            run(Command::new("ditto").args(["-x", "-k"]).arg(&archive).arg(&out))?;
            let app = find_app(&out)?;
            verify(&app)?;
            Ok(app)
        }
        Target::Linux { .. } => {
            run(Command::new("tar").arg("-xzf").arg(&archive).arg("-C").arg(&out))?;
            let bin = out.join("ebobo-steamdeck").join("ebobo");
            if bin.is_file() {
                Ok(bin)
            } else {
                Err(format!("{asset} has no ebobo-steamdeck/ebobo"))
            }
        }
    }
}

fn find_app(dir: &Path) -> Result<PathBuf, String> {
    std::fs::read_dir(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e == "app"))
        .ok_or_else(|| "no .app in the downloaded zip".to_string())
}

/// Signature is intact and made by our Developer ID team.
fn verify_mac(app: &Path) -> Result<(), String> {
    run(Command::new("codesign").args(["--verify", "--deep", "--strict"]).arg(app))?;
    run(Command::new("codesign").args([MAC_REQUIREMENT, "--verify"]).arg(app))?;
    Ok(())
}

/// Replaces `bundle` with `new` (same volume): verify, old aside, new in place, old deleted.
/// On failure the old bundle is put back.
fn swap_mac(bundle: &Path, new: &Path, verify: impl Fn(&Path) -> Result<(), String>) -> Result<(), String> {
    verify(new)?;
    let name = bundle.file_name().ok_or("bundle has no name")?.to_string_lossy();
    let aside = bundle.with_file_name(format!(".{name}.old-{}", std::process::id()));
    std::fs::rename(bundle, &aside).map_err(|e| format!("move {} aside: {e}", bundle.display()))?;
    if let Err(e) = std::fs::rename(new, bundle) {
        let _ = std::fs::rename(&aside, bundle);
        return Err(format!("move new bundle into place: {e}"));
    }
    let _ = std::fs::remove_dir_all(&aside);
    Ok(())
}

/// Atomically renames `new` over `exe` (mode 755).
fn swap_linux(exe: &Path, new: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(new, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("chmod {}: {e}", new.display()))?;
    }
    std::fs::rename(new, exe).map_err(|e| format!("replace {}: {e}", exe.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ebobo-update-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn update_version_parse_and_order() {
        assert_eq!(Version::parse("v0.2.3"), Some(Version(0, 2, 3)));
        assert_eq!(Version::parse(" 1.10.0 "), Some(Version(1, 10, 0)));
        assert_eq!(Version::parse("0.3.0-rc1"), Some(Version(0, 3, 0)));
        for bad in ["", "v1.2", "1.2.3.4", "x.1.2", "v"] {
            assert_eq!(Version::parse(bad), None, "{bad}");
        }
        assert!(Version(0, 2, 10) > Version(0, 2, 9));
        assert!(Version(0, 10, 0) > Version(0, 9, 99));
        assert!(Version(1, 0, 0) > Version(0, 99, 99));
        assert_eq!(Version(0, 2, 3).tag(), "v0.2.3");
    }

    #[test]
    fn update_location_header() {
        let h = "HTTP/2 302 \r\ndate: x\r\nLocation: https://github.com/idoo/ebobo/releases/tag/v0.2.3\r\n\r\n";
        assert_eq!(parse_location(h), Some(Version(0, 2, 3)));
        let lower = "location: https://github.com/idoo/ebobo/releases/tag/v1.0.0\n";
        assert_eq!(parse_location(lower), Some(Version(1, 0, 0)));
        assert_eq!(parse_location("HTTP/2 200\r\ncontent-type: text/html\r\n"), None);
        assert_eq!(parse_location("location: https://github.com/idoo/ebobo/releases\r\n"), None);
    }

    #[test]
    fn update_sha256sums_parse_and_mismatch() {
        let dir = tmp("sums");
        let file = dir.join("ebobo-mac.zip");
        std::fs::write(&file, b"abc").unwrap();
        let abc = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        let sums = format!(
            "{}  ebobo-steamdeck.tar.gz\n{}  ebobo-mac.zip\n",
            "0".repeat(64),
            abc.to_uppercase()
        );
        assert_eq!(expected_sha(&sums, "ebobo-mac.zip").as_deref(), Some(abc));
        assert_eq!(expected_sha(&format!("{abc} *dist/ebobo-mac.zip"), "ebobo-mac.zip").as_deref(), Some(abc));
        assert_eq!(expected_sha("nothex  ebobo-mac.zip", "ebobo-mac.zip"), None);
        assert!(check_sha(&sums, "ebobo-mac.zip", &file).is_ok());
        let err = check_sha(&sums, "ebobo-steamdeck.tar.gz", &file).unwrap_err();
        assert!(err.contains("mismatch"), "{err}");
        assert!(check_sha(&sums, "install-steamdeck.sh", &file).unwrap_err().contains("not listed"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn update_linux_swap_replaces_fake_exe() {
        let dir = tmp("linux");
        let exe = dir.join("ebobo");
        std::fs::write(&exe, b"old").unwrap();
        let new = dir.join(".ebobo-update-1/new/ebobo-steamdeck/ebobo");
        std::fs::create_dir_all(new.parent().unwrap()).unwrap();
        std::fs::write(&new, b"new").unwrap();
        swap_linux(&exe, &new).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"new");
        assert!(!new.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&exe).unwrap().permissions().mode() & 0o777, 0o755);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn update_mac_swap_with_injected_verifier() {
        let dir = tmp("mac");
        let bundle = dir.join("ebobo.app");
        std::fs::create_dir_all(bundle.join("Contents/MacOS")).unwrap();
        std::fs::write(bundle.join("Contents/MacOS/ebobo"), b"old").unwrap();
        let new = dir.join(".ebobo-update-1/new/ebobo.app");
        std::fs::create_dir_all(new.join("Contents/MacOS")).unwrap();
        std::fs::write(new.join("Contents/MacOS/ebobo"), b"new").unwrap();

        // Rejected: nothing moves.
        let err = swap_mac(&bundle, &new, |_| Err("unsigned".into())).unwrap_err();
        assert_eq!(err, "unsigned");
        assert_eq!(std::fs::read(bundle.join("Contents/MacOS/ebobo")).unwrap(), b"old");
        assert!(new.exists());

        swap_mac(&bundle, &new, |p| if p == new { Ok(()) } else { Err("wrong path".into()) }).unwrap();
        assert_eq!(std::fs::read(bundle.join("Contents/MacOS/ebobo")).unwrap(), b"new");
        assert!(!new.exists());
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".old-"))
            .collect();
        assert!(leftovers.is_empty(), "old bundle deleted");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn update_mac_target_and_blockers() {
        let exe = Path::new("/Applications/ebobo.app/Contents/MacOS/ebobo");
        assert_eq!(target_for(exe), Some(Target::Mac { bundle: "/Applications/ebobo.app".into() }));
        assert_eq!(target_for(Path::new("/Users/x/code/ebobo/target/debug/ebobo")), None);
        let translocated = Target::Mac {
            bundle: "/private/var/folders/x/AppTranslocation/ABC/d/ebobo.app".into(),
        };
        assert_eq!(blocker(&translocated), Some(Blocker::MoveToApplications));
        let readonly = Target::Mac { bundle: "/System/ebobo.app".into() };
        assert_eq!(blocker(&readonly), Some(Blocker::MoveToApplications));
        let dir = tmp("writable");
        assert_eq!(blocker(&Target::Mac { bundle: dir.join("ebobo.app") }), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An unsigned bundle never passes the Mac verifier.
    #[cfg(target_os = "macos")]
    #[test]
    fn update_mac_verifier_rejects_unsigned_bundle() {
        let dir = tmp("unsigned");
        let app = dir.join("ebobo.app");
        std::fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        std::fs::copy(std::env::current_exe().unwrap(), app.join("Contents/MacOS/ebobo")).unwrap();
        std::fs::write(
            app.join("Contents/Info.plist"),
            "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>CFBundleExecutable</key>\
             <string>ebobo</string><key>CFBundleIdentifier</key><string>se.jvcr.ebobo</string></dict></plist>",
        )
        .unwrap();
        let err = verify_mac(&app).unwrap_err();
        eprintln!("rejected as expected: {err}");
        // Even with an ad-hoc signature (a valid seal), our team requirement must fail.
        let _ = Command::new("codesign").args(["-s", "-", "--force"]).arg(&app).output();
        let err = verify_mac(&app).unwrap_err();
        eprintln!("ad-hoc rejected as expected: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Network: the real latest release downloads, (if it has SHA256SUMS) verifies, unpacks and
    /// passes the signature check. `cargo test -- --ignored update_e2e --nocapture`.
    #[test]
    #[ignore]
    fn update_e2e_real_release() {
        let latest = latest_version().expect("curl reaches GitHub");
        eprintln!("latest release: v{latest}");
        let target = if cfg!(target_os = "macos") {
            Target::Mac { bundle: "/nonexistent/ebobo.app".into() }
        } else {
            Target::Linux { exe: "/nonexistent/ebobo".into() }
        };
        let dir = tmp("e2e");
        match fetch_and_unpack(&target, latest, &dir, verify_mac) {
            Ok(path) => {
                eprintln!("downloaded, checksum ok, unpacked and verified: {}", path.display());
                if let Target::Mac { .. } = target {
                    verify_mac(&path).expect("signed by 6NE65QKPJ6");
                }
            }
            Err(err) => {
                eprintln!("refused: {err}");
                assert!(err.contains("no SHA256SUMS"), "only a release without SHA256SUMS may be refused: {err}");
                // Prove the rest of the path on the asset itself: download + signature check.
                let archive = dir.join(target.asset());
                download(&format!("{REPO}/download/{}/{}", latest.tag(), target.asset()), &archive).unwrap();
                eprintln!("{} sha256 {}", target.asset(), sha256_file(&archive).unwrap());
                if let Target::Mac { .. } = target {
                    let out = dir.join("new");
                    std::fs::create_dir_all(&out).unwrap();
                    run(Command::new("ditto").args(["-x", "-k"]).arg(&archive).arg(&out)).unwrap();
                    let app = find_app(&out).unwrap();
                    verify_mac(&app).expect("real release passes the signature check");
                    eprintln!("signature ok: {}", app.display());
                }
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
