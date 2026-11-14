//! Steam Input pin for the Steam Deck's Desktop Mode.
//!
//! There Steam keeps non-Steam apps on its Desktop Layout even when they are started from Steam
//! (ValveSoftware/steam-for-linux#10551, #13314): the buttons type keys and no gamepad exists.
//! While the window has focus this pins Steam Input to ebobo's own shortcut with
//! `steam://forceinputappid/<appid>`, re-issues the pin 4 s later (Steam overrides the first one)
//! and hands the Deck back to the Desktop Layout on focus loss. A guard process releases the pin
//! when ebobo ends for any reason, a crash or kill included.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Sender};
use std::time::Duration;

/// Steam's pseudo-app for the Desktop Layout; forcing it switches back at once.
const DESKTOP_LAYOUT: u32 = 413080;
/// Steam overrides a pin issued right at focus; the second one sticks.
const REPIN: Duration = Duration::from_secs(4);
/// Delay of the re-pin after a sleep, once the controller has re-registered.
const WAKE_REPIN: Duration = Duration::from_secs(1);
/// A longer gap between ticks means the machine slept.
const SLEEP_GAP: Duration = Duration::from_secs(2);
/// Keyboard keys before this are start-up noise, not a missing gamepad.
const HINT_AFTER: Duration = Duration::from_secs(6);
/// Nesting bound of the VDF parser.
const MAX_DEPTH: u32 = 16;

/// Removed from the environment of every `steam` call: with them the client mistakes the call
/// for the game itself or fails to load.
const STRIPPED_ENV: [&str; 5] = [
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "SteamAppId",
    "SteamGameId",
    "STEAM_COMPAT_APP_ID",
];

/// Blocks until ebobo's end of the pipe closes, then releases the pin if Steam still runs:
/// the Desktop Layout switches the Deck back, `0` clears the pin.
#[cfg(unix)]
const GUARD_SCRIPT: &str = r#"cat >/dev/null
pid=$(cat "$HOME/.steam/steam.pid" 2>/dev/null)
[ -n "$pid" ] && [ -d "/proc/$pid" ] || exit 0
steam steam://forceinputappid/413080
steam steam://forceinputappid/0"#;

/// SteamOS desktop session: not gamescope (Game Mode), where Steam Input already works.
fn is_desktop_session(
    steam_os: bool,
    gamescope_display: Option<&str>,
    current_desktop: Option<&str>,
) -> bool {
    steam_os
        && gamescope_display.is_none()
        && !current_desktop.is_some_and(|d| d.eq_ignore_ascii_case("gamescope"))
}

/// Appid of a non-Steam shortcut from its `SteamGameId` (`appid << 32 | flags`); a plain appid
/// belongs to a real Steam app, not to ebobo's shortcut.
fn appid_from_game_id(game_id: &str) -> Option<u32> {
    let id: u64 = game_id.trim().parse().ok()?;
    (id > u64::from(u32::MAX)).then_some((id >> 32) as u32)
}

fn force_url(id: u32) -> String {
    format!("steam://forceinputappid/{id}")
}

enum Vdf {
    Map(Vec<(String, Vdf)>),
    Str(String),
    Int(u32),
    Other,
}

impl Vdf {
    fn get(&self, key: &str) -> Option<&Vdf> {
        let Vdf::Map(entries) = self else { return None };
        entries.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    fn str(&self, key: &str) -> Option<&str> {
        match self.get(key)? {
            Vdf::Str(s) => Some(s),
            _ => None,
        }
    }
}

/// NUL-terminated string at `*pos`; advances past the NUL.
fn vdf_cstr(data: &[u8], pos: &mut usize) -> Option<String> {
    let rest = data.get(*pos..)?;
    let len = rest.iter().position(|&b| b == 0)?;
    *pos += len + 1;
    Some(String::from_utf8_lossy(&rest[..len]).into_owned())
}

/// Binary VDF map at `*pos`: entries `type, key\0, value` up to the byte `0x08`. Keys are
/// lowercased. `None` on truncation, an unknown type or nesting beyond `MAX_DEPTH`.
fn vdf_map(data: &[u8], pos: &mut usize, depth: u32) -> Option<Vdf> {
    if depth > MAX_DEPTH {
        return None;
    }
    let mut entries = Vec::new();
    loop {
        let kind = *data.get(*pos)?;
        *pos += 1;
        if kind == 0x08 {
            return Some(Vdf::Map(entries));
        }
        let key = vdf_cstr(data, pos)?.to_lowercase();
        let value = match kind {
            0x00 => vdf_map(data, pos, depth + 1)?,
            0x01 => Vdf::Str(vdf_cstr(data, pos)?),
            0x02 => {
                let bytes = data.get(*pos..pos.checked_add(4)?)?;
                *pos += 4;
                Vdf::Int(u32::from_le_bytes(bytes.try_into().ok()?))
            }
            0x07 => {
                data.get(*pos..pos.checked_add(8)?)?;
                *pos += 8;
                Vdf::Other
            }
            _ => return None,
        };
        entries.push((key, value));
    }
}

/// Appid of the first shortcut in a `shortcuts.vdf` named `ebobo` whose exe file is `ebobo`.
fn shortcut_appid(data: &[u8]) -> Option<u32> {
    let root = vdf_map(data, &mut 0, 0)?;
    let Vdf::Map(shortcuts) = root.get("shortcuts")? else { return None };
    shortcuts.iter().find_map(|(_, shortcut)| {
        let exe = shortcut.str("exe")?.trim().trim_matches('"');
        let ours = shortcut.str("appname")?.eq_ignore_ascii_case("ebobo")
            && Path::new(exe).file_name().is_some_and(|name| name == "ebobo");
        match shortcut.get("appid")? {
            Vdf::Int(appid) if ours => Some(*appid),
            _ => None,
        }
    })
}

/// Appid from the `shortcuts.vdf` of every Steam user, the most recently written first.
fn appid_from_shortcuts(home: &Path) -> Option<u32> {
    let users = std::fs::read_dir(home.join(".steam/steam/userdata")).ok()?;
    let mut files: Vec<_> = users
        .flatten()
        .filter_map(|user| {
            let path = user.path().join("config/shortcuts.vdf");
            let modified = std::fs::metadata(&path).ok()?.modified().ok()?;
            Some((modified, path))
        })
        .collect();
    files.sort_by(|a, b| b.0.cmp(&a.0));
    files.iter().find_map(|(_, path)| shortcut_appid(&std::fs::read(path).ok()?))
}

/// The Steam client is up: its pid file names a live process. Running `steam` otherwise would
/// start the client.
fn steam_running(home: &Path) -> bool {
    std::fs::read_to_string(home.join(".steam/steam.pid"))
        .ok()
        .and_then(|pid| pid.trim().parse::<u32>().ok())
        .is_some_and(|pid| Path::new("/proc").join(pid.to_string()).exists())
}

/// `cmd` without `STRIPPED_ENV` and with null stdio.
fn detached(mut cmd: Command) -> Command {
    for name in STRIPPED_ENV {
        cmd.env_remove(name);
    }
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    cmd
}

/// Write end of the guard's stdin; the guard fires when it closes.
#[cfg(unix)]
type Guard = Option<std::process::ChildStdin>;
#[cfg(not(unix))]
type Guard = ();

/// Starts the guard process in its own process group, so a closing terminal does not kill it.
/// It is never waited on.
#[cfg(unix)]
fn spawn_guard() -> Guard {
    use std::os::unix::process::CommandExt;
    let mut sh = Command::new("sh");
    sh.arg("-c").arg(GUARD_SCRIPT);
    let mut sh = detached(sh);
    sh.stdin(Stdio::piped()).process_group(0);
    sh.spawn().ok()?.stdin.take()
}

#[cfg(not(unix))]
fn spawn_guard() -> Guard {}

/// Runs the queued `steam://forceinputappid/<id>` calls strictly in order. Holds the guard's
/// pipe until the channel closes or the process dies. After a spawn failure, logged once,
/// further sends are dropped.
fn worker(home: PathBuf, ids: mpsc::Receiver<u32>) {
    let mut guard: Option<Guard> = None;
    let mut failed = false;
    for id in ids {
        if failed || !steam_running(&home) {
            continue;
        }
        if guard.is_none() {
            guard = Some(spawn_guard());
        }
        let mut steam = Command::new("steam");
        steam.arg(force_url(id));
        if let Err(err) = detached(steam).status() {
            eprintln!("ebobo: steam input: {err}");
            failed = true;
        }
    }
}

/// Focus state machine: which id to send when. Without an appid it never sends.
struct Pin {
    appid: Option<u32>,
    focused: bool,
    repin: Option<Duration>,
}

impl Pin {
    fn new(appid: Option<u32>) -> Pin {
        Pin { appid, focused: false, repin: None }
    }

    /// The id to send on a focus change, if any.
    fn set_focus(&mut self, focused: bool) -> Option<u32> {
        let appid = self.appid?;
        if focused == self.focused {
            return None;
        }
        self.focused = focused;
        if focused {
            self.repin = Some(REPIN);
            Some(appid)
        } else {
            self.repin = None;
            Some(DESKTOP_LAYOUT)
        }
    }

    /// The id to send when the re-pin timer runs out. A `dt` above `SLEEP_GAP` arms a re-pin
    /// `WAKE_REPIN` later: Steam drops the pin when the controller re-registers after a sleep.
    fn tick(&mut self, dt: Duration) -> Option<u32> {
        let appid = self.appid?;
        if !self.focused {
            return None;
        }
        if dt > SLEEP_GAP {
            self.repin = Some(WAKE_REPIN);
            return None;
        }
        let left = self.repin?;
        if left <= dt {
            self.repin = None;
            Some(appid)
        } else {
            self.repin = Some(left - dt);
            None
        }
    }
}

/// Pins Steam Input to ebobo's own Steam shortcut while the window has focus. Inert outside a
/// SteamOS desktop session.
pub struct SteamInput {
    desktop: bool,
    pin: Pin,
    home: Option<PathBuf>,
    /// The worker thread, started with the first send.
    tx: Option<Sender<u32>>,
}

impl SteamInput {
    /// `steam_os`: this is SteamOS / a Steam Deck (the caller's `config::is_steam_deck()`).
    pub fn new(steam_os: bool) -> SteamInput {
        let env = |name: &str| std::env::var(name).ok();
        let desktop = is_desktop_session(
            steam_os,
            env("GAMESCOPE_WAYLAND_DISPLAY").as_deref(),
            env("XDG_CURRENT_DESKTOP").as_deref(),
        );
        let home = std::env::var_os("HOME").map(PathBuf::from).filter(|_| desktop);
        let appid = home.as_deref().and_then(|home| {
            env("SteamGameId")
                .and_then(|id| appid_from_game_id(&id))
                .or_else(|| appid_from_shortcuts(home))
        });
        SteamInput { desktop, pin: Pin::new(appid), home, tx: None }
    }

    /// SteamOS desktop session (not Game Mode), whether or not pinning is possible.
    pub fn deck_desktop(&self) -> bool {
        self.desktop
    }

    /// The window gained or lost input focus. Idempotent: called every frame with the current state.
    pub fn set_focus(&mut self, focused: bool) {
        if let Some(id) = self.pin.set_focus(focused) {
            self.send(id);
        }
    }

    /// Once per main-loop iteration; `dt` = time since the previous call.
    pub fn tick(&mut self, dt: Duration) {
        if let Some(id) = self.pin.tick(dt) {
            self.send(id);
        }
    }

    fn send(&mut self, id: u32) {
        let Some(home) = &self.home else { return };
        let tx = self.tx.get_or_insert_with(|| {
            let (tx, rx) = mpsc::channel();
            let home = home.clone();
            std::thread::spawn(move || worker(home, rx));
            tx
        });
        let _ = tx.send(id);
    }
}

/// Notices a player stuck on the keyboard: on the Deck that means the buttons type keys.
#[derive(Default)]
pub struct KeysHint {
    elapsed: Duration,
    silenced: bool,
}

impl KeysHint {
    /// True exactly once: a keyboard key went down at least 6 s after start while no gamepad button has ever been pressed. A gamepad press at any time silences it for good.
    pub fn update(&mut self, key_down: bool, pad_used: bool, dt: Duration) -> bool {
        self.elapsed = self.elapsed.saturating_add(dt);
        self.silenced |= pad_used;
        if self.silenced || !key_down || self.elapsed < HINT_AFTER {
            return false;
        }
        self.silenced = true;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const APPID: u32 = 3199158233;

    fn vdf_str(out: &mut Vec<u8>, key: &str, value: &str) {
        out.push(0x01);
        out.extend_from_slice(key.as_bytes());
        out.push(0);
        out.extend_from_slice(value.as_bytes());
        out.push(0);
    }

    fn vdf_int(out: &mut Vec<u8>, key: &str, value: u32) {
        out.push(0x02);
        out.extend_from_slice(key.as_bytes());
        out.push(0);
        out.extend_from_slice(&value.to_le_bytes());
    }

    fn vdf_open(out: &mut Vec<u8>, key: &str) {
        out.push(0x00);
        out.extend_from_slice(key.as_bytes());
        out.push(0);
    }

    /// A `shortcuts.vdf` with the given `(appid, AppName, Exe)` shortcuts.
    fn shortcuts_vdf(shortcuts: &[(u32, &str, &str)]) -> Vec<u8> {
        let mut out = Vec::new();
        vdf_open(&mut out, "shortcuts");
        for (i, (appid, name, exe)) in shortcuts.iter().enumerate() {
            vdf_open(&mut out, &i.to_string());
            vdf_int(&mut out, "appid", *appid);
            vdf_str(&mut out, "AppName", name);
            vdf_str(&mut out, "Exe", exe);
            vdf_str(&mut out, "StartDir", "/home/deck");
            out.push(0x07);
            out.extend_from_slice(b"LastPlayTime\0");
            out.extend_from_slice(&[0xAB; 8]);
            vdf_open(&mut out, "tags");
            vdf_str(&mut out, "0", "favorite");
            out.push(0x08);
            out.push(0x08);
        }
        out.extend_from_slice(&[0x08, 0x08]);
        out
    }

    const EBOBO_EXE: &str = "\"/home/deck/Applications/ebobo/ebobo\"";

    #[test]
    fn vdf_finds_ebobo_shortcut() {
        let data = shortcuts_vdf(&[
            (2789400123, "Other Game", "\"/usr/bin/other\""),
            (APPID, "ebobo", EBOBO_EXE),
        ]);
        assert_eq!(shortcut_appid(&data), Some(APPID));
        let upper = shortcuts_vdf(&[(APPID, "Ebobo", "/opt/ebobo")]);
        assert_eq!(shortcut_appid(&upper), Some(APPID));
    }

    #[test]
    fn vdf_skips_other_exe() {
        let data = shortcuts_vdf(&[(7, "ebobo", "\"/home/deck/Applications/ebobo/ebobo-old\"")]);
        assert_eq!(shortcut_appid(&data), None);
        let data = shortcuts_vdf(&[
            (7, "ebobo", "\"/usr/bin/flatpak\""),
            (APPID, "ebobo", EBOBO_EXE),
        ]);
        assert_eq!(shortcut_appid(&data), Some(APPID));
        assert_eq!(shortcut_appid(&shortcuts_vdf(&[])), None);
    }

    #[test]
    fn vdf_survives_truncation_and_garbage() {
        let data = shortcuts_vdf(&[
            (2789400123, "Other Game", "\"/usr/bin/other\""),
            (APPID, "ebobo", EBOBO_EXE),
        ]);
        for len in 0..data.len() {
            assert_eq!(shortcut_appid(&data[..len]), None, "length {len}");
        }
        assert_eq!(shortcut_appid(&[0xFF; 64]), None);
        assert_eq!(shortcut_appid(b"\"shortcuts\"\n{\n}\n"), None);
        assert_eq!(shortcut_appid(&[0x02, b'a', 0, 1, 2]), None);
        assert_eq!(shortcut_appid(&[0x07, b'a', 0, 1, 2, 3]), None);
        let mut deep = Vec::new();
        for _ in 0..10_000 {
            vdf_open(&mut deep, "shortcuts");
        }
        assert_eq!(shortcut_appid(&deep), None);
        let garbage: Vec<u8> = (0..4096u32).map(|i| (i.wrapping_mul(2654435761) >> 24) as u8).collect();
        for start in 0..garbage.len() {
            assert_eq!(shortcut_appid(&garbage[start..]), None);
        }
    }

    #[test]
    fn game_id_to_appid() {
        assert_eq!(u64::from(APPID) << 32 | 0x0200_0000, 13740279985497702400);
        assert_eq!(appid_from_game_id("13740279985497702400"), Some(APPID));
        assert_eq!(appid_from_game_id("0"), None);
        assert_eq!(appid_from_game_id("570"), None);
        assert_eq!(appid_from_game_id("4294967295"), None);
        assert_eq!(appid_from_game_id(""), None);
        assert_eq!(appid_from_game_id("ebobo"), None);
        assert_eq!(appid_from_game_id("-1"), None);
    }

    #[test]
    fn desktop_session_table() {
        assert!(is_desktop_session(true, None, None));
        assert!(is_desktop_session(true, None, Some("KDE")));
        assert!(!is_desktop_session(true, Some("gamescope-0"), None));
        assert!(!is_desktop_session(true, Some("gamescope-0"), Some("KDE")));
        assert!(!is_desktop_session(true, None, Some("gamescope")));
        assert!(!is_desktop_session(true, None, Some("Gamescope")));
        assert!(!is_desktop_session(false, None, None));
        assert!(!is_desktop_session(false, None, Some("KDE")));
    }

    #[test]
    fn url_and_env() {
        assert_eq!(force_url(APPID), "steam://forceinputappid/3199158233");
        assert_eq!(force_url(DESKTOP_LAYOUT), "steam://forceinputappid/413080");
        for name in ["LD_PRELOAD", "LD_LIBRARY_PATH", "SteamAppId", "SteamGameId", "STEAM_COMPAT_APP_ID"] {
            assert!(STRIPPED_ENV.contains(&name), "{name}");
        }
        assert_eq!(STRIPPED_ENV.len(), 5);
    }

    const FRAME: Duration = Duration::from_millis(100);

    fn ticks(pin: &mut Pin, sent: &mut Vec<u32>, n: u32) {
        for _ in 0..n {
            sent.extend(pin.tick(FRAME));
        }
    }

    #[test]
    fn focus_pins_and_repins() {
        let mut pin = Pin::new(Some(APPID));
        let mut sent = Vec::new();
        ticks(&mut pin, &mut sent, 100);
        assert!(sent.is_empty());

        sent.extend(pin.set_focus(true));
        assert_eq!(sent, [APPID]);
        for _ in 0..5 {
            sent.extend(pin.set_focus(true));
        }
        assert_eq!(sent, [APPID]);

        ticks(&mut pin, &mut sent, 39);
        assert_eq!(sent, [APPID]);
        ticks(&mut pin, &mut sent, 1);
        assert_eq!(sent, [APPID, APPID]);
        ticks(&mut pin, &mut sent, 600);
        sent.extend(pin.set_focus(true));
        assert_eq!(sent, [APPID, APPID]);

        sent.extend(pin.set_focus(false));
        sent.extend(pin.set_focus(false));
        assert_eq!(sent, [APPID, APPID, DESKTOP_LAYOUT]);
        ticks(&mut pin, &mut sent, 100);
        assert_eq!(sent.len(), 3);
    }

    #[test]
    fn focus_loss_disarms_repin() {
        let mut pin = Pin::new(Some(APPID));
        let mut sent = Vec::new();
        sent.extend(pin.set_focus(true));
        ticks(&mut pin, &mut sent, 10);
        sent.extend(pin.set_focus(false));
        ticks(&mut pin, &mut sent, 100);
        assert_eq!(sent, [APPID, DESKTOP_LAYOUT]);
    }

    #[test]
    fn sleep_repins_a_second_later() {
        let mut pin = Pin::new(Some(APPID));
        let mut sent = Vec::new();
        sent.extend(pin.set_focus(true));
        ticks(&mut pin, &mut sent, 100);
        assert_eq!(sent, [APPID, APPID]);

        sent.extend(pin.tick(Duration::from_secs(3)));
        ticks(&mut pin, &mut sent, 9);
        assert_eq!(sent, [APPID, APPID]);
        ticks(&mut pin, &mut sent, 1);
        assert_eq!(sent, [APPID, APPID, APPID]);
        ticks(&mut pin, &mut sent, 100);
        assert_eq!(sent.len(), 3);

        sent.extend(pin.set_focus(false));
        sent.extend(pin.tick(Duration::from_secs(3)));
        ticks(&mut pin, &mut sent, 100);
        assert_eq!(sent, [APPID, APPID, APPID, DESKTOP_LAYOUT]);
    }

    #[test]
    fn inert_never_sends() {
        let mut pin = Pin::new(None);
        let mut sent = Vec::new();
        sent.extend(pin.set_focus(true));
        ticks(&mut pin, &mut sent, 100);
        sent.extend(pin.tick(Duration::from_secs(3)));
        ticks(&mut pin, &mut sent, 100);
        sent.extend(pin.set_focus(false));
        assert!(sent.is_empty());

        let mut input = SteamInput::new(false);
        assert!(!input.deck_desktop());
        input.set_focus(true);
        input.tick(Duration::from_secs(5));
        input.set_focus(false);
        assert!(input.tx.is_none());
    }

    #[test]
    fn keys_hint() {
        let s = Duration::from_secs;
        let mut hint = KeysHint::default();
        assert!(!hint.update(true, false, s(5)));
        assert!(!hint.update(false, false, s(1)));
        assert!(hint.update(true, false, s(1)));
        assert!(!hint.update(true, false, s(1)));
        assert!(!hint.update(true, false, s(60)));

        let mut hint = KeysHint::default();
        assert!(!hint.update(false, true, s(1)));
        assert!(!hint.update(true, false, s(10)));
        assert!(!hint.update(true, false, s(10)));

        let mut hint = KeysHint::default();
        assert!(!hint.update(true, true, s(10)));
        assert!(!hint.update(true, false, s(1)));
    }
}
