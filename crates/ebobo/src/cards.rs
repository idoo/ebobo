//! The library screen: a grid of game cards with filter chips and search. Owner: cards slice.
//!
//! Self-contained: its own state, input handling (buttons, keys, pointer), layout and
//! drawing. `ui.rs` hands it the frame's input and the list of games and acts on what it
//! returns; the pictures come from `ebobo_art::Art`, favourites and history from
//! `ebobo_art::shelf::Shelf`.
