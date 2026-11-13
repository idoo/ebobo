# ebobo

A NES emulator for playing with friends over the internet, on a Mac or a Steam Deck. Up to four players.

It uses rollback netcode, like fighting games do. A bad connection makes a sprite jump now and then, and the game never slows down.

No games included. Bring your own ROMs.

## Install

**Mac** (macOS 11+): download [ebobo-mac.zip](https://github.com/idoo/ebobo/releases/latest/download/ebobo-mac.zip), unzip it, drag ebobo.app to Applications.

**Steam Deck**: in Desktop Mode open Konsole and run

```sh
curl -sL https://github.com/idoo/ebobo/releases/latest/download/install-steamdeck.sh | bash
```

ebobo then shows up in Steam under Non-Steam. The same line works on other x86_64 Linux.

It updates itself on launch, so everyone stays on the same version.

## Playing

The host picks Create room and gets a five-letter code. Friends pick Join by code and type it in. They don't need the ROM. The host sends it over and it stays in their library.

If someone can't get in, Settings > Advanced > Network check usually tells you why.

ROMs are found in `~/ROMs`, `~/Downloads`, EmuDeck's folder and a few other usual places. You can also drop one on the window.

## Keys

Arrows to move, X is A, Z is B, Enter is Start, right Shift is Select, Esc opens the menu. Hold Tab to fast-forward and Backspace to rewind. F5 and F7 save and load. Gamepads just work.

## Building

```sh
cargo run --release -p ebobo
scripts/fetch-test-roms.sh && cargo test
```

## Your own server

Rooms go through a small Cloudflare Worker in `worker/`. ebobo uses mine by default. To run your own (the free Cloudflare plan is enough), install Node.js and run:

```sh
scripts/deploy-server.sh
```

It signs you in to Cloudflare, deploys and prints the server URL. Everyone who plays together enters that URL in Settings > Advanced > Server, or starts ebobo with `--server <url>`.

## License

MIT.
