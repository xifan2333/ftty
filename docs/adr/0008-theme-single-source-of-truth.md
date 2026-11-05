# ADR 0008: Single Source of Truth for Palette and Default Colors

## Context
The 256-entry palette and the default foreground/background lived in two places: `Terminal` (active palette, active defaults, configured snapshot, plus a `palette_dirty` boolean) and `AppState` (a mirrored palette, `default_fg`, `default_bg`). Five call sites hand-synchronised the mirror — construction, `SessionOutcome::palette_changed` handling, render setup, `SIGUSR1` reload, and per-frame colour-scheme construction — so any missed path left the window rendering a stale palette while OSC 4/10/11 queries answered with the new one. Reset sequences (OSC 104/110/111, DECSTR) also had to remember to flip the dirty flag separately from mutating the state.

## Decision
We introduce `Theme` under `src/theme.rs` as the sole owner of the active palette, active default colours, the configured snapshot that reset sequences restore, and a monotonic `revision`. `Terminal` holds one `theme` field; `AppState` holds none. The session reports `SessionOutcome::theme_changed` by comparing the revision it last observed, replacing the `palette_dirty` boolean. `ColorScheme` gains `From<&Theme>` so rendering borrows the single instance.

## Consequences
- Seven duplicated fields collapse into one module; zero hand-synchronised copies remain.
- Dirty tracking is derived from a monotonic revision instead of a mutable boolean that every mutation site has to remember to set.
- Palette mutation paths (OSC 4/10/11/104/110/111, DECSTR, `SIGUSR1` reload) all funnel through `Theme` mutators, which is the single place a change can be observed or tested.
