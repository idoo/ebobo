//! Command line parsing (no dependencies).

use std::path::PathBuf;

use crate::config::{Config, ScreenMode};
use crate::crt::Effect;
use crate::enhance::Mode;

pub const USAGE: &str = "\
usage: ebobo [ROM] [options]

  ROM                  play a NES or Mega Drive game (.nes .md .gen .smd .bin, or a .zip of
                       one) alone or with local pads
  --host ROM           create an online room for ROM and show its code
  --join CODE          join an online room by its code
  --listen ROM         host by IP: wait for a player on UDP --port (default 7777)
  --port P             UDP port for --listen
  --connect ADDR       join a game hosted by IP (host:port)
  --relay              always relay through the server (no direct UDP)
  --server URL         signaling server
  --name NICK          nickname shown to other players
  --fullscreen         start fullscreen
  --windowed           start in a window
  --screenshot PATH    write a PNG of the screen (effects included) when the app exits
  --screenshot-size WxH  size of that PNG (default: the window's drawable size)
  --effect NAME        picture effect: off, crt, pvm, lcd
  --enhance MODE       picture enhancement: off, hd, depth (HD smoothing plus depth shadows)
  --ambient on|off     ambient light in the bars around the game's picture
  --screen MODE        screen mode: stretch, fit (aspect kept), pixel (integer scale)
  --library            start in the library of games (for screenshots and checks)
  --exit-after SECONDS quit after this many seconds
  --fresh              start from power-on and keep no moment
  --autostart N        host: start the game once N players are in the room
  --resume FROM        host: start from new, last (the last moment) or save instead of the
                       default (the last moment, else the save, else a new game); a missing
                       file means a new game
  --seats N            online: how many players sit at this device (1-4)
  --saves WHO          host: who may save and load online: all, host, off
  --script LIST        netplay: press F5 (save) / F7 (load) for everyone at advanced frames,
                       e.g. 600=save,900=load
  --version            print the version
  --help               show this help

Automation: a host prints `ebobo: room CODE`; a netplay game prints
`ebobo: game started ...` and `ebobo: frame N hash H` every 300 frames. Online saves print
`ebobo: resumed last|save sha S` (a game started from a state), `ebobo: net save by slot N
sha S` (the save file written), `ebobo: net load by slot N`, `ebobo: last moment sha S`
(the last-moment file written from a checkpoint) and `ebobo: last moment on leave sha S`
(written when the game is left); S = first 16 hex digits of the SHA-256 of the state bytes.";

/// `--resume`: what a host starts from instead of the default choice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResumeFrom {
    New,
    Last,
    Save,
}

/// A `--script` step: the hotkey pressed (F5 save / F7 load for everyone).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScriptKey {
    Save,
    Load,
}

/// What to do right after startup.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Start {
    #[default]
    Menu,
    Solo(PathBuf),
    Host(PathBuf),
    Join(String),
    Listen(PathBuf),
    Connect(String),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Cli {
    pub start: Start,
    pub port: Option<u16>,
    pub relay: bool,
    pub server: Option<String>,
    pub name: Option<String>,
    pub fullscreen: Option<bool>,
    pub screenshot: Option<PathBuf>,
    pub screenshot_size: Option<(u32, u32)>,
    pub effect: Option<Effect>,
    pub enhance: Option<Mode>,
    pub ambient: Option<bool>,
    pub screen: Option<ScreenMode>,
    /// `--library`: open the library over the main menu (only without a game to start).
    pub library: bool,
    pub exit_after: Option<f64>,
    /// `--fresh`: solo games neither continue from their last moment nor write it.
    pub fresh: bool,
    pub autostart: Option<usize>,
    pub resume: Option<ResumeFrom>,
    /// `--seats`: players at this device in an online room.
    pub seats: Option<u8>,
    /// `--saves`: who may save and load in a room this device hosts, for this run.
    pub saves: Option<ebobo_net::SavePolicy>,
    /// `--script` steps sorted by frame.
    pub script: Vec<(u64, ScriptKey)>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Parsed {
    Run(Cli),
    Help,
    Version,
}

impl Cli {
    /// CLI overrides go into the in-memory config only; the app never saves them.
    pub fn apply(&self, config: &mut Config) {
        if let Some(port) = self.port {
            config.direct_port = port;
        }
        if self.relay {
            config.force_relay = true;
        }
        if let Some(server) = &self.server {
            config.server = server.clone();
        }
        if let Some(name) = &self.name {
            config.nickname = name.clone();
        }
        if let Some(fullscreen) = self.fullscreen {
            config.fullscreen = fullscreen;
        }
        if let Some(effect) = self.effect {
            config.effect = effect;
        }
        if let Some(enhance) = self.enhance {
            config.enhance = enhance;
        }
        if let Some(ambient) = self.ambient {
            config.ambient = ambient;
        }
        if let Some(screen) = self.screen {
            config.screen = screen;
        }
        if let Some(saves) = self.saves {
            config.online_saves = saves.as_str().to_string();
        }
    }
}

/// Parses arguments (without the program name). `Err` holds a one-line reason; the caller
/// prints it with the usage and exits with status 2.
pub fn parse(args: &[String]) -> Result<Parsed, String> {
    let mut cli = Cli::default();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v.to_string())),
            _ => (arg.as_str(), None),
        };
        let mut value = |name: &str| -> Result<String, String> {
            match inline.clone().or_else(|| it.next().cloned()) {
                Some(v) if !v.is_empty() => Ok(v),
                _ => Err(format!("{name} needs a value")),
            }
        };
        match flag {
            "--help" | "-h" => return Ok(Parsed::Help),
            "--version" | "-V" => return Ok(Parsed::Version),
            "--host" => set_start(&mut cli, Start::Host(value(flag)?.into()))?,
            "--listen" => set_start(&mut cli, Start::Listen(value(flag)?.into()))?,
            "--join" => {
                let raw = value(flag)?;
                let code = ebobo_net::normalize_code(&raw).ok_or_else(|| format!("bad room code: {raw}"))?;
                set_start(&mut cli, Start::Join(code))?
            }
            "--connect" => set_start(&mut cli, Start::Connect(value(flag)?))?,
            "--port" => {
                let raw = value(flag)?;
                let port = raw.parse::<u16>().ok().filter(|p| *p != 0).ok_or_else(|| format!("bad port: {raw}"))?;
                cli.port = Some(port);
            }
            "--relay" => cli.relay = true,
            "--server" => cli.server = Some(value(flag)?),
            "--name" => {
                let name = value(flag)?;
                cli.name = Some(name.chars().take(16).collect());
            }
            "--fullscreen" => cli.fullscreen = Some(true),
            "--windowed" => cli.fullscreen = Some(false),
            "--screenshot" => cli.screenshot = Some(value(flag)?.into()),
            "--screenshot-size" => {
                let raw = value(flag)?;
                let size = raw
                    .split_once(['x', 'X'])
                    .and_then(|(w, h)| Some((w.trim().parse::<u32>().ok()?, h.trim().parse::<u32>().ok()?)))
                    .filter(|&(w, h)| (1..=8192).contains(&w) && (1..=8192).contains(&h))
                    .ok_or_else(|| format!("bad size: {raw}"))?;
                cli.screenshot_size = Some(size);
            }
            "--enhance" => {
                let raw = value(flag)?;
                cli.enhance = Some(Mode::parse(&raw).ok_or_else(|| format!("bad enhancement: {raw} (off, hd, depth)"))?);
            }
            "--ambient" => {
                let raw = value(flag)?;
                cli.ambient = Some(match raw.trim().to_ascii_lowercase().as_str() {
                    "on" => true,
                    "off" => false,
                    _ => return Err(format!("bad --ambient: {raw} (on, off)")),
                });
            }
            "--screen" => {
                let raw = value(flag)?;
                cli.screen = Some(match raw.trim().to_ascii_lowercase().as_str() {
                    "stretch" => ScreenMode::Stretch,
                    "fit" => ScreenMode::Fit,
                    "pixel" => ScreenMode::Pixel,
                    _ => return Err(format!("bad screen mode: {raw} (stretch, fit, pixel)")),
                });
            }
            "--library" => cli.library = true,
            "--fresh" => cli.fresh = true,
            "--effect" => {
                let raw = value(flag)?;
                cli.effect = Some(Effect::parse(&raw).ok_or_else(|| format!("bad effect: {raw} (off, crt, pvm, lcd)"))?);
            }
            "--exit-after" => {
                let raw = value(flag)?;
                let secs = raw
                    .parse::<f64>()
                    .ok()
                    .filter(|s| s.is_finite() && *s >= 0.0)
                    .ok_or_else(|| format!("bad number of seconds: {raw}"))?;
                cli.exit_after = Some(secs);
            }
            "--autostart" => {
                let raw = value(flag)?;
                let n = raw.parse::<usize>().ok().filter(|n| (2..=4).contains(n)).ok_or_else(|| format!("bad player count: {raw}"))?;
                cli.autostart = Some(n);
            }
            "--resume" => {
                let raw = value(flag)?;
                cli.resume = Some(match raw.as_str() {
                    "new" => ResumeFrom::New,
                    "last" => ResumeFrom::Last,
                    "save" => ResumeFrom::Save,
                    _ => return Err(format!("bad --resume: {raw} (new, last, save)")),
                });
            }
            "--seats" => {
                let raw = value(flag)?;
                let n = raw.parse::<u8>().ok().filter(|n| (1..=4).contains(n)).ok_or_else(|| format!("bad --seats: {raw} (1-4)"))?;
                cli.seats = Some(n);
            }
            "--saves" => {
                let raw = value(flag)?;
                cli.saves = Some(match raw.as_str() {
                    "all" => ebobo_net::SavePolicy::Everyone,
                    "host" => ebobo_net::SavePolicy::Host,
                    "off" => ebobo_net::SavePolicy::Off,
                    _ => return Err(format!("bad --saves: {raw} (all, host, off)")),
                });
            }
            "--script" => cli.script = parse_script(&value(flag)?)?,
            _ if flag.starts_with('-') && flag.len() > 1 => return Err(format!("unknown option: {flag}")),
            _ => set_start(&mut cli, Start::Solo(arg.into()))?,
        }
        if inline.is_some() && matches!(flag, "--relay" | "--fullscreen" | "--windowed" | "--library" | "--fresh") {
            return Err(format!("{flag} takes no value"));
        }
    }
    if cli.port.is_some() && !matches!(cli.start, Start::Listen(_)) {
        return Err("--port only works with --listen".into());
    }
    if cli.library && cli.start != Start::Menu {
        return Err("--library starts in the library: give no ROM, --host, --join, --listen or --connect".into());
    }
    Ok(Parsed::Run(cli))
}

fn set_start(cli: &mut Cli, start: Start) -> Result<(), String> {
    if cli.start != Start::Menu {
        return Err("give only one of ROM, --host, --join, --listen, --connect".into());
    }
    cli.start = start;
    Ok(())
}

/// `600=save,900=load` into steps sorted by frame.
fn parse_script(raw: &str) -> Result<Vec<(u64, ScriptKey)>, String> {
    let mut steps = raw
        .split(',')
        .map(|step| {
            let (frame, key) = step.trim().split_once('=').ok_or_else(|| format!("bad --script step: {step} (FRAME=save|load)"))?;
            let frame = frame.trim().parse::<u64>().map_err(|_| format!("bad --script frame: {frame}"))?;
            let key = match key.trim() {
                "save" => ScriptKey::Save,
                "load" => ScriptKey::Load,
                other => return Err(format!("bad --script key: {other} (save, load)")),
            };
            Ok((frame, key))
        })
        .collect::<Result<Vec<_>, String>>()?;
    steps.sort_by_key(|&(frame, _)| frame);
    Ok(steps)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Result<Parsed, String> {
        parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    fn run(args: &[&str]) -> Cli {
        match p(args) {
            Ok(Parsed::Run(cli)) => cli,
            other => panic!("{args:?}: {other:?}"),
        }
    }

    #[test]
    fn empty_is_menu() {
        assert_eq!(run(&[]), Cli::default());
    }

    #[test]
    fn modes() {
        assert_eq!(run(&["mario.nes"]).start, Start::Solo("mario.nes".into()));
        assert_eq!(run(&["--host", "a.nes"]).start, Start::Host("a.nes".into()));
        assert_eq!(run(&["--join", "ab-cd e"]).start, Start::Join("ABCDE".into()));
        let cli = run(&["--listen", "a.nes", "--port", "9000"]);
        assert_eq!((cli.start, cli.port), (Start::Listen("a.nes".into()), Some(9000)));
        assert_eq!(run(&["--connect=1.2.3.4:7777"]).start, Start::Connect("1.2.3.4:7777".into()));
    }

    #[test]
    fn options() {
        let cli = run(&[
            "--relay", "--server", "http://x", "--name", "Vanya", "--windowed", "--screenshot", "s.png",
            "--exit-after", "2.5", "g.nes",
        ]);
        assert!(cli.relay);
        assert_eq!(cli.server.as_deref(), Some("http://x"));
        assert_eq!(cli.name.as_deref(), Some("Vanya"));
        assert_eq!(cli.fullscreen, Some(false));
        assert_eq!(cli.screenshot, Some("s.png".into()));
        assert_eq!(cli.exit_after, Some(2.5));
        assert_eq!(run(&["--fullscreen"]).fullscreen, Some(true));
        assert_eq!(p(&["--help"]), Ok(Parsed::Help));
        assert_eq!(p(&["x.nes", "--version"]), Ok(Parsed::Version));
    }

    #[test]
    fn errors() {
        for bad in [
            &["--frobnicate"][..],
            &["-x"],
            &["--host"],
            &["--join", "IIIII"],
            &["--port", "70000", "--listen", "a.nes"],
            &["--port", "9000"],
            &["a.nes", "b.nes"],
            &["--host", "a.nes", "--join", "ABCDE"],
            &["--exit-after", "soon"],
            &["--exit-after", "-1"],
            &["--relay=yes"],
        ] {
            assert!(p(bad).is_err(), "{bad:?}");
        }
        assert_eq!(p(&["--wat"]), Err("unknown option: --wat".into()));
    }

    #[test]
    fn apply_overrides() {
        let mut config = Config::default();
        run(&["--listen", "a.nes", "--port", "9001", "--relay", "--name", "N", "--fullscreen"]).apply(&mut config);
        assert_eq!(config.direct_port, 9001);
        assert!(config.force_relay && config.fullscreen);
        assert_eq!(config.nickname, "N");
        assert_eq!(config.effect, Effect::Off);
        let cli = run(&["--effect", "CRT", "--screenshot-size", "1280x800"]);
        cli.apply(&mut config);
        assert_eq!(config.effect, Effect::CrtTv);
        assert_eq!(cli.screenshot_size, Some((1280, 800)));
        assert!(p(&["--effect", "vhs"]).is_err());
        assert!(p(&["--screenshot-size", "0x5"]).is_err());
    }

    #[test]
    fn picture_and_library_options() {
        let cli = run(&["--enhance", "HD", "--ambient=off", "--screen", "fit", "--library"]);
        assert_eq!((cli.enhance, cli.ambient, cli.screen, cli.library), (Some(Mode::Hd), Some(false), Some(ScreenMode::Fit), true));
        assert_eq!(run(&["--enhance=depth"]).enhance, Some(Mode::Depth));
        assert_eq!(run(&["--enhance", "off"]).enhance, Some(Mode::Off));
        assert_eq!(run(&["--ambient", "on"]).ambient, Some(true));
        assert_eq!(run(&["--screen", "pixel"]).screen, Some(ScreenMode::Pixel));
        assert_eq!(run(&["--screen", "Stretch"]).screen, Some(ScreenMode::Stretch));
        let mut config = Config::default();
        run(&["--enhance", "depth", "--ambient", "off", "--screen", "pixel"]).apply(&mut config);
        assert_eq!((config.enhance, config.ambient, config.screen), (Mode::Depth, false, ScreenMode::Pixel));
        let mut kept = Config::default();
        run(&[]).apply(&mut kept);
        assert_eq!(kept, Config::default());
        for bad in [
            &["--enhance", "4k"][..],
            &["--enhance"],
            &["--ambient", "yes"],
            &["--ambient"],
            &["--screen", "zoom"],
            &["--screen"],
            &["--library=1"],
            &["--library", "a.nes"],
        ] {
            assert!(p(bad).is_err(), "{bad:?}");
        }
        for flag in ["--enhance", "--ambient", "--screen", "--library"] {
            assert!(USAGE.contains(flag), "{flag}");
        }
    }

    #[test]
    fn resume_and_script() {
        assert_eq!(run(&["--host", "a.nes"]).resume, None);
        assert_eq!(run(&["--resume", "new"]).resume, Some(ResumeFrom::New));
        assert_eq!(run(&["--resume=last"]).resume, Some(ResumeFrom::Last));
        assert_eq!(run(&["--resume", "save"]).resume, Some(ResumeFrom::Save));
        assert_eq!(
            run(&["--script", "900=load, 600=save,1200=save"]).script,
            [(600, ScriptKey::Save), (900, ScriptKey::Load), (1200, ScriptKey::Save)]
        );
        assert_eq!(run(&["--script=600=save"]).script, [(600, ScriptKey::Save)]);
        for bad in [
            &["--resume", "old"][..],
            &["--resume"],
            &["--script"],
            &["--script", "600"],
            &["--script", "x=save"],
            &["--script", "600=reset"],
            &["--script", "600=save,"],
            &["--script", "-5=save"],
        ] {
            assert!(p(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn seats_and_saves() {
        use ebobo_net::SavePolicy;
        let none = run(&["--host", "a.nes"]);
        assert_eq!((none.seats, none.saves), (None, None));
        assert_eq!(run(&["--seats", "2"]).seats, Some(2));
        assert_eq!(run(&["--seats=4"]).seats, Some(4));
        assert_eq!(run(&["--saves", "all"]).saves, Some(SavePolicy::Everyone));
        assert_eq!(run(&["--saves=host"]).saves, Some(SavePolicy::Host));
        let mut config = Config::default();
        run(&["--saves", "off"]).apply(&mut config);
        assert_eq!(config.online_saves, "off");
        for bad in [&["--seats", "0"][..], &["--seats", "5"], &["--seats", "two"], &["--seats"], &["--saves", "guests"], &["--saves"]] {
            assert!(p(bad).is_err(), "{bad:?}");
        }
    }
}
