# ebobo

A NES emulator for macOS and Linux.

## Building

```sh
cargo run --release -p ebobo -- path/to/game.nes
```

SDL2 is built from source and linked statically, so a C compiler and CMake are needed.

## Keys

Arrows to move, X is A, Z is B, Enter is Start, right Shift is Select, Esc opens the menu. Gamepads just work.
