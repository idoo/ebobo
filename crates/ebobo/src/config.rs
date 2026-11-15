//! Settings file and app directories (lead-owned; shared by app and ui).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const DEFAULT_SERVER: &str = "https://ebobo-signal.mc-se.workers.dev";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Shown to other players (≤ 16 chars).
    pub nickname: String,
    /// Signaling server base URL.
    pub server: String,
    /// Extra ROM folders on top of `library::default_dirs()`.
    pub rom_dirs: Vec<PathBuf>,
    pub fullscreen: bool,
    /// Window scale in windowed mode (1-6).
    pub scale: u32,
    /// How the picture fills the window or screen.
    pub screen: ScreenMode,
    /// Picture effect (CRT TV, PVM monitor, LCD); `Off` = plain pixels.
    pub effect: crate::crt::Effect,
    /// GGRS input delay when hosting; `None` = auto from ping.
    pub input_delay: Option<u8>,
    /// Never try direct UDP, always relay through the server.
    pub force_relay: bool,
    /// UDP port for "host by IP".
    pub direct_port: u16,
    /// Last address typed in "join by IP".
    pub last_join_addr: String,
    /// "ru" or "en"; `None` = system language.
    pub lang: Option<String>,
    /// 0.0-1.0.
    pub volume: f32,
    /// Fast-forward button toggles instead of acting only while held.
    pub ff_toggle: bool,
    /// Hardware 8-sprites-per-line limit (display only; each peer may differ).
    pub sprite_limit: SpriteLimit,
    /// Check GitHub for a newer ebobo on start and install it (Settings ▸ Advanced).
    pub auto_update: bool,
    /// Download box art for the library's cards (Settings ▸ Advanced); off shows only what is cached.
    pub online_art: bool,
}

/// "Sprite limit" setting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SpriteLimit {
    /// Removed while 3-4 players play (Four Score on), hardware otherwise.
    #[default]
    Auto,
    /// Like the NES: 8 sprites per line.
    Hardware,
    /// All sprites drawn.
    Off,
}

impl SpriteLimit {
    /// Whether the hardware limit applies (`Nes::set_sprite_limit`) for this game.
    pub fn applies(self, four_score: bool) -> bool {
        match self {
            SpriteLimit::Auto => !four_score,
            SpriteLimit::Hardware => true,
            SpriteLimit::Off => false,
        }
    }
}

/// "Screen" setting: how the 256x240 picture is scaled to the window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScreenMode {
    /// Fills the whole window, aspect ignored (no bars).
    #[default]
    Stretch,
    /// NES aspect kept, as large as fits (bars on one axis).
    Fit,
    /// Integer scale when it fits, letterboxed.
    Pixel,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            nickname: default_nickname(),
            server: DEFAULT_SERVER.to_string(),
            rom_dirs: Vec::new(),
            fullscreen: is_steam_deck(),
            scale: 3,
            screen: ScreenMode::Stretch,
            effect: crate::crt::Effect::Off,
            input_delay: None,
            force_relay: false,
            direct_port: ebobo_net::DEFAULT_DIRECT_PORT,
            last_join_addr: String::new(),
            lang: None,
            volume: 0.8,
            ff_toggle: false,
            sprite_limit: SpriteLimit::Auto,
            auto_update: true,
            online_art: true,
        }
    }
}

impl Config {
    /// Reads `config.toml`; missing or broken files give defaults (a broken file is kept as
    /// `config.toml.bad` so the user's edits are not silently lost).
    pub fn load() -> Config {
        let path = config_dir().join("config.toml");
        match std::fs::read_to_string(&path) {
            Ok(text) => match toml::from_str(&text) {
                Ok(config) => config,
                Err(err) => {
                    eprintln!("ebobo: {}: {err}; using defaults", path.display());
                    let _ = std::fs::rename(&path, path.with_extension("toml.bad"));
                    Config::default()
                }
            },
            Err(_) => Config::default(),
        }
    }

    pub fn save(&self) {
        let dir = config_dir();
        let text = toml::to_string_pretty(self).expect("config serializes");
        if let Err(err) = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(dir.join("config.toml"), text)) {
            eprintln!("ebobo: cannot save settings: {err}");
        }
    }
}

fn project_dirs() -> Option<directories::ProjectDirs> {
    directories::ProjectDirs::from("se", "jvcr", "ebobo")
}

/// macOS: ~/Library/Application Support/se.jvcr.ebobo; Linux: ~/.config/ebobo.
pub fn config_dir() -> PathBuf {
    project_dirs().map(|d| d.config_dir().to_path_buf()).unwrap_or_else(|| PathBuf::from("."))
}

/// macOS: same as config; Linux: ~/.local/share/ebobo. Holds `saves/` and `roms/`.
pub fn data_dir() -> PathBuf {
    project_dirs().map(|d| d.data_dir().to_path_buf()).unwrap_or_else(|| PathBuf::from("."))
}

/// Battery saves (`<rom stem>.sav`).
pub fn saves_dir() -> PathBuf {
    data_dir().join("saves")
}

/// Screenshots: macOS `~/Pictures/ebobo`; Linux `$XDG_PICTURES_DIR/ebobo` (else
/// `~/Pictures/ebobo`); Windows the Pictures library.
pub fn pictures_dir() -> PathBuf {
    let dirs = directories::UserDirs::new();
    let pictures = dirs
        .as_ref()
        .and_then(|d| d.picture_dir().map(PathBuf::from))
        .or_else(|| dirs.as_ref().map(|d| d.home_dir().join("Pictures")))
        .unwrap_or_else(|| PathBuf::from("."));
    pictures.join("ebobo")
}

/// `path` with the home directory shown as `~` (for toasts).
pub fn tilde(path: &std::path::Path) -> String {
    match directories::UserDirs::new().and_then(|d| path.strip_prefix(d.home_dir()).ok().map(PathBuf::from)) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// Steam Deck / SteamOS / gamescope session: start fullscreen, gamepad-first UI, built-in keyboard.
pub fn is_steam_deck() -> bool {
    std::env::var_os("SteamDeck").is_some()
        || std::env::var_os("GAMESCOPE_WAYLAND_DISPLAY").is_some()
        || std::fs::read_to_string("/etc/os-release").is_ok_and(|s| s.lines().any(|l| l.trim() == "ID=steamos"))
}

const NICK_MAX: usize = 16;

/// First-run nickname: the Steam persona on SteamOS, else the OS user's full name, else the
/// login name ("Player" for none or `deck`); at most 16 characters.
fn default_nickname() -> String {
    let steam = || {
        let home = directories::UserDirs::new()?.home_dir().to_path_buf();
        let vdf = std::fs::read_to_string(home.join(".steam/steam/config/loginusers.vdf")).ok()?;
        steam_persona(&vdf)
    };
    let picked = if is_steam_deck() { steam() } else { None };
    if let Some(name) = picked.or_else(os_full_name).map(|n| trunc_nick(&n)).filter(|n| !n.is_empty()) {
        return name;
    }
    let user = std::env::var("USER").or_else(|_| std::env::var("USERNAME")).unwrap_or_default();
    let name: String = user.chars().filter(|c| c.is_alphanumeric()).take(NICK_MAX).collect();
    if name.is_empty() || name == "deck" { "Player".to_string() } else { name }
}

fn trunc_nick(name: &str) -> String {
    name.trim().chars().filter(|c| !c.is_control()).take(NICK_MAX).collect::<String>().trim().to_string()
}

/// `PersonaName` of the `"MostRecent" "1"` user in Steam's `loginusers.vdf`.
fn steam_persona(vdf: &str) -> Option<String> {
    // Each user is a `"<steamid>" { "Key" "Value" ... }` block; values are quoted pairs.
    let mut persona = None;
    let mut recent = false;
    let mut depth = 0;
    for line in vdf.lines() {
        let line = line.trim();
        match line {
            "{" => depth += 1,
            "}" => {
                if depth == 2 && recent {
                    return persona;
                }
                if depth == 2 {
                    persona = None;
                    recent = false;
                }
                depth -= 1;
            }
            _ if depth == 2 => {
                let parts: Vec<&str> = line.split('"').collect();
                // `"Key"\t\t"Value"` splits into ["", Key, ws, Value, ""].
                if parts.len() >= 5 {
                    match parts[1] {
                        "PersonaName" => persona = Some(parts[3].to_string()),
                        "MostRecent" => recent = parts[3] == "1",
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// The OS user's full name: macOS `id -F`, else the passwd GECOS field.
fn os_full_name() -> Option<String> {
    if cfg!(target_os = "macos") {
        let out = std::process::Command::new("id").arg("-F").output().ok()?;
        let name = String::from_utf8(out.stdout).ok()?.trim().to_string();
        return (out.status.success() && !name.is_empty()).then_some(name);
    }
    let user = std::env::var("USER").ok()?;
    let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
    let line = passwd.lines().find(|l| l.split(':').next() == Some(user.as_str()))?;
    let gecos = line.split(':').nth(4)?.split(',').next()?.trim();
    (!gecos.is_empty()).then(|| gecos.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steam_persona_picks_most_recent() {
        let vdf = r#""users"
{
	"76561198000000001"
	{
		"AccountName"		"old"
		"PersonaName"		"Old Name"
		"MostRecent"		"0"
	}
	"76561198000000002"
	{
		"AccountName"		"ivan"
		"PersonaName"		"Ваня из Стокгольма длинный"
		"RememberPassword"		"1"
		"MostRecent"		"1"
		"Timestamp"		"1731500000"
	}
}
"#;
        let name = steam_persona(vdf).unwrap();
        assert_eq!(name, "Ваня из Стокгольма длинный");
        assert_eq!(trunc_nick(&name), "Ваня из Стокголь");
        assert_eq!(steam_persona(&vdf.replace("\"MostRecent\"\t\t\"1\"", "\"MostRecent\"\t\t\"0\"")), None);
    }


    #[test]
    fn screen_mode_defaults_and_round_trips() {
        let old: Config = toml::from_str("fullscreen = true\nscale = 2\n").unwrap();
        assert_eq!(old.screen, ScreenMode::Stretch);
        let fit: Config = toml::from_str("screen = \"fit\"\n").unwrap();
        assert_eq!(fit.screen, ScreenMode::Fit);
        let text = toml::to_string_pretty(&Config { screen: ScreenMode::Pixel, ..Config::default() }).unwrap();
        assert!(text.contains("screen = \"pixel\""));
    }

    #[test]
    fn old_buttons_key_is_ignored() {
        // 0.3.1 wrote a "Buttons" setting; the key is gone and old files still load.
        let old: Config = toml::from_str("buttons = \"bottom-right\"\n").unwrap();
        assert_eq!(old, Config::default());
        let text = toml::to_string_pretty(&Config::default()).unwrap();
        assert!(!text.contains("buttons"));
    }

    #[test]
    fn online_art_defaults_on_for_old_files() {
        let old: Config = toml::from_str("fullscreen = true\nscale = 2\nauto_update = false\n").unwrap();
        assert!(old.online_art);
        let off: Config = toml::from_str("online_art = false\n").unwrap();
        assert!(!off.online_art);
        let text = toml::to_string_pretty(&Config { online_art: false, ..Config::default() }).unwrap();
        assert!(!toml::from_str::<Config>(&text).unwrap().online_art);
    }

    #[test]
    fn effect_defaults_and_round_trips() {
        use crate::crt::Effect;
        let old: Config = toml::from_str("fullscreen = true\nscale = 2\nscreen = \"fit\"\n").unwrap();
        assert_eq!(old.effect, Effect::Off);
        for e in Effect::ALL {
            let text = toml::to_string_pretty(&Config { effect: e, ..Config::default() }).unwrap();
            let back: Config = toml::from_str(&text).unwrap();
            assert_eq!(back.effect, e);
        }
    }
}
