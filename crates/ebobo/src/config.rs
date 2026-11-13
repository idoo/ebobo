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
            input_delay: None,
            force_relay: false,
            direct_port: ebobo_net::DEFAULT_DIRECT_PORT,
            last_join_addr: String::new(),
            lang: None,
            volume: 0.8,
            ff_toggle: false,
            sprite_limit: SpriteLimit::Auto,
            auto_update: true,
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

/// Steam Deck / gamescope session: start fullscreen, gamepad-first UI.
pub fn is_steam_deck() -> bool {
    std::env::var_os("SteamDeck").is_some() || std::env::var_os("GAMESCOPE_WAYLAND_DISPLAY").is_some()
}

fn default_nickname() -> String {
    let user = std::env::var("USER").or_else(|_| std::env::var("USERNAME")).unwrap_or_default();
    let name: String = user.chars().filter(|c| c.is_alphanumeric()).take(16).collect();
    if name.is_empty() || name == "deck" { "Player".to_string() } else { name }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screen_mode_defaults_and_round_trips() {
        let old: Config = toml::from_str("fullscreen = true\nscale = 2\n").unwrap();
        assert_eq!(old.screen, ScreenMode::Stretch);
        let fit: Config = toml::from_str("screen = \"fit\"\n").unwrap();
        assert_eq!(fit.screen, ScreenMode::Fit);
        let text = toml::to_string_pretty(&Config { screen: ScreenMode::Pixel, ..Config::default() }).unwrap();
        assert!(text.contains("screen = \"pixel\""));
    }
}
