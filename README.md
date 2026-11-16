# ebobo

A NES and Sega Mega Drive / Genesis emulator for playing with friends over the internet, on a Mac, a Windows PC or a Steam Deck. Up to four players. Both cores are written from scratch for this.

It uses rollback netcode, like fighting games do. A bad connection makes a sprite jump now and then, and the game never slows down. The input delay follows the connection during the game: up to three frames on a slow link, the rest is rollback. If the Wi-Fi drops or a phone hotspot switches networks mid-game, ebobo reroutes through the server within a few seconds, goes back to a direct link when one works again, and waits up to 20 seconds for a player to come back. Someone who dropped out for longer joins again with the same code and the game goes on.

No games included. Bring your own ROMs. The Mega Drive core is new and so far checked on free homebrew and demos only: if a game misbehaves, open an issue.

## Install

**Mac** (macOS 11+): download [ebobo-mac.zip](https://github.com/idoo/ebobo/releases/latest/download/ebobo-mac.zip), unzip it, drag ebobo.app to Applications.

**Windows** (10 or later, 64-bit): download [ebobo-windows.zip](https://github.com/idoo/ebobo/releases/latest/download/ebobo-windows.zip), unzip it and run ebobo.exe. Windows may warn about an unknown publisher: More info, then Run anyway. Put your ROMs in a `ROMs` folder in your home folder (`C:\Users\<you>\ROMs`).

**Steam Deck**: in Desktop Mode open Konsole and run

```sh
curl -sL https://github.com/idoo/ebobo/releases/latest/download/install-steamdeck.sh | bash
```

ebobo then shows up in Steam under Non-Steam. The same line works on other x86_64 Linux.

It updates itself on launch, so everyone stays on the same version.

## Playing

ebobo opens on your games. Pick one to play alone: it continues where you left it last time (Reset in the menu starts over), and it pauses while its window is behind another one. Four save slots keep a picture of the moment each was saved.

To play online, open a game's menu (Start on a gamepad, Space, a right click) and pick Play online, or pick Create room in the main menu. You get a five-letter code (Cmd+C or Ctrl+C copies it). Friends pick Join by code and type or paste it in. They don't need the ROM. The host sends it over and it stays in their library.

Two to four people can share one device in a room: set Players here to how many of you sit at it, and each takes a gamepad (the keyboard is the first player). A room holds four players in all. The host can move players to other controller numbers: select a player in the room and press Left or Right.

The room stays open while you play. A friend who joins late, or comes back after dropping out, brings everyone back to the room for a few seconds, and the game goes on from the same moment with them in it. The host can also go back to the room from the menu, for example to change the seats.

If someone can't get in, Settings > Advanced > Network check usually tells you why.

Online games can be saved too, from the menu (F5, F7). The host picks in the room who may do it: everyone, only the host, or nobody. Next time the host makes a room with the same game, it continues where you stopped, or from the save.

ROMs (`.nes`, `.md`, `.gen`, `.smd`, `.bin`, or any of them in a `.zip`) are found in `~/ROMs`, `~/Downloads`, EmuDeck's folders and a few other usual places. You can also drop one on the window.

The library shows games as cards with their box art, which ebobo looks up by the ROM's checksum, so file names don't matter. A hack or homebrew gets its own title screen instead. Filter by console, recently played or favourites, or just start typing to search. Settings > Advanced turns the downloads off.

Start on a gamepad opens a game's menu (Space, a right click or a long press do the same): add it to favourites, give it your own name, hide it from the list, or delete its file for good (your saves stay). Hidden games wait under the crossed-out eye, where the same menu puts them back.

On the Steam Deck everything works by touch, and text fields bring up ebobo's own keyboard (Steam's keyboard can't type into it in Game Mode). Your Steam name is the default nickname. In Desktop Mode Steam makes the Deck's buttons type keys; ebobo switches them to a gamepad while its window is in front and back when you leave.

Settings > Enhance redraws the pixels of NES and Mega Drive games in HD: smooth outlines instead of squares, with sprites kept apart from the background, and optionally soft shadows that characters, platforms and scenery cast on what is behind them. Settings > Effect turns the picture into an old CRT TV (curved glass, scanlines, glow), a flat studio monitor, or a handheld LCD. The two combine. When the picture leaves bars at the sides, they glow in its colours.

## Keys

Arrows to move, X is A, Z is B, Enter is Start, right Shift is Select, Esc opens the menu. Hold Tab to fast-forward and Backspace to rewind. F5 and F7 save and load. F12 (Select+Y on a gamepad) saves a screenshot to Pictures/ebobo, online too.

On a gamepad A and Y are A, B and X are B (PlayStation: Cross and Triangle, Circle and Square), so any two neighbouring buttons work. L1 and R1 are turbo B and turbo A, L2 rewinds, R2 fast-forwards. A click of the right stick opens the menu; so does holding Select+Start, or a tap or click on the picture. PlayStation, Xbox and Switch pads work over USB or Bluetooth; player 1 is whoever presses a button first after the game starts.

Mega Drive games use the six-button layout: X, A, B on the pad are A, B, C (PlayStation: Square, Cross, Circle), Y is Y, L1 and R1 are X and Z, Select is Mode. On the keyboard Z X C are A B C and A S D are X Y Z.

## Building

```sh
cargo run --release -p ebobo
scripts/fetch-test-roms.sh && scripts/fetch-md-test-roms.sh && cargo test
```

The CPU cores are checked against the SingleStepTests suites (`scripts/fetch-m68k-tests.sh`, `scripts/fetch-z80-tests.sh`), and the Mega Drive core against a reference emulator run as a black box (`tools/md-oracle`).

## Your own server

Rooms go through a small Cloudflare Worker in `worker/`. ebobo uses mine by default. To run your own (the free Cloudflare plan is enough), install Node.js and run:

```sh
scripts/deploy-server.sh
```

It signs you in to Cloudflare, deploys and prints the server URL. Everyone who plays together enters that URL in Settings > Advanced > Server, or starts ebobo with `--server <url>`.

## License

MIT. The HD filter is a port of [MMPX](https://casual-effects.com/research/McGuire2021PixelArt/) by Morgan McGuire and Mara Gagiu (MIT); rollback is [GGRS](https://github.com/gschup/ggrs) (MIT or Apache-2.0), patched in `vendor/ggrs`.
