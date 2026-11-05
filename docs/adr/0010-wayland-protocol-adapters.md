# ADR 0010: Split the Wayland Client into Protocol Adapters

## Context
`src/wayland.rs` had grown to 1380 lines holding five unrelated protocol families:
global advertisement, the XDG shell surface lifecycle, seat/keyboard/pointer/cursor
shape input, `text-input-v3`, and the data-device clipboard. Twenty-three
`impl Dispatch<X, ()> for AppState` blocks were flattened into one file, so reading
any single protocol meant scrolling past four others, and protocol knowledge was
interleaved with `AppState` orchestration (PTY writes, redraws, cursor shapes).

## Decision
We split the Wayland client into a directory module where each protocol extension owns
one adapter:

- `wayland::registry` — `wl_registry` global advertisement plus the compositor and
  output adapters that only bind and track globals.
- `wayland::window` — `wl_surface`, `wl_callback`, the XDG shell family, and the
  fractional-scale/viewport extensions, all of which shape surface geometry.
- `wayland::input` — `wl_seat`, `wl_keyboard`, `wl_pointer`, and `wp_cursor_shape`,
  i.e. every device the seat hands out.
- `wayland::pointer` — the `PointerController` bridges: layout context construction,
  declarative outcome dispatch, and hover/cursor-shape application.
- `wayland::ime` — `text-input-v3` activation, preedit/commit staging, and the cursor
  box that positions the candidate window.
- `wayland::clipboard` — the data-device family, MIME negotiation, and text transfer.

`wayland/mod.rs` keeps only shared state: `WaylandState`, `OfferData`, `WindowState`
with its lifecycle state machine, the `WaylandState` constructors that are not owned by
a protocol (state defaults, scaling predicate, window transitions), and module
declarations. Each lazy `init_*` constructor now lives beside the adapter that owns the
object it creates, so "where does this proxy come from" is answered in one file.

The split is a pure move: no event handling changed. Dispatch impls are legal in any
module of the crate, so callers and wire-level tests cross the same seam as before.

## Consequences
- The largest adapter is 298 lines; `mod.rs` drops from 1380 to 192 lines.
- Protocol knowledge is local: reading `ime.rs` no longer requires reading clipboard code,
  and a compositor extension can be added or deleted in one file.
- The empty compatibility shims `wayland::init`, `wayland::text_input`, and
  `wayland::xdg` were removed. `wayland::seat` survives as a documented re-export because the grid
  helpers it exposes live in `crate::input::pointer`, not in this module.
