//! Input handling: keyboard, mouse, selection, and IME.
//!
//! Keybindings and xkb keysym parsing live in [`keys`], the stateful key-event
//! translator in [`keyboard`], and the cursor/selection and pointer state
//! machines in their own modules. Only the items callers address by name are
//! re-exported here.

mod keyboard;
mod keys;

pub mod ime;
pub mod mouse;
pub mod pointer;
pub mod selection;

pub use keyboard::KeyboardHandler;
pub use keys::{
    KeyAction, KittyKeyboardFlags, Modifiers, canonicalize_sym, is_modifier_key, parse_key_combo,
};
pub use pointer::{
    PointerContext, PointerController, PointerOutcome, find_url_at_col, find_url_in_grid,
    x11_button_index,
};
