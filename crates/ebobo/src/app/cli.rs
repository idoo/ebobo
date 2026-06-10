//! Command line parsing (no dependencies).

use std::path::PathBuf;

use crate::config::Config;

pub const USAGE: &str = "\
usage: ebobo [ROM] [options]

  ROM                  play a .nes/.zip file alone (or with local pads)
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
  --screenshot PATH    write a PNG of the screen when the app exits
  --exit-after SECONDS quit after this many seconds
  --autostart N        host: start the game once N players are in the room
  --version            print the version
  --help               show this help

Automation: a host prints `ebobo: room CODE`; a netplay game prints
`ebobo: game started ...` and `ebobo: frame N hash H` every 300 frames.";

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
    pub exit_after: Option<f64>,
    pub autostart: Option<usize>,
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
            _ if flag.starts_with('-') && flag.len() > 1 => return Err(format!("unknown option: {flag}")),
            _ => set_start(&mut cli, Start::Solo(arg.into()))?,
        }
        if inline.is_some() && matches!(flag, "--relay" | "--fullscreen" | "--windowed") {
            return Err(format!("{flag} takes no value"));
        }
    }
    if cli.port.is_some() && !matches!(cli.start, Start::Listen(_)) {
        return Err("--port only works with --listen".into());
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
    }
}
