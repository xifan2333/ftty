# ADR 0009: Layer the Font Subsystem into Atlas, Cache, and Manager

## Context
`src/font.rs` had grown to 1902 lines mixing four orthogonal concerns: shelf-packed atlas bitmap packing, FreeType face loading and rasterization, Fontconfig-driven fallback resolution, and roughly 490 lines of persistent on-disk cache serialization with its own magic, version, and entry limits. Debugging a fallback or packing problem meant reading past mtime validation and binary (de)serialization code, and the cache format could only be exercised through the full font-loading path.

## Decision
We split the font subsystem into a directory module with three seams. `font::atlas` owns `GlyphAtlas`, `Shelf`, and `CachedGlyph`; `font::cache` owns `CachedFontFile`, `FontCacheData`, the cache magic/version/limits, path resolution, directory fingerprinting, and serialization; `font/mod.rs` retains face loading, fallback resolution, and `FontManager`. `mod.rs` re-exports the public atlas and cache surface so existing call sites are unchanged.

## Consequences
- The cache format's magic, version, and validation rules live behind one interface, so a version bump is a single-module change.
- Atlas packing has no disk or Fontconfig dependency and can be exercised with synthetic bitmaps.
- `font.rs` (now `font/mod.rs`) drops from 1902 to 1179 lines, leaving face loading, fallback chains, and metrics as the readable core.
