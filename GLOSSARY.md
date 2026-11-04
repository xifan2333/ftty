# ftty Domain Glossary

Canonical vocabulary for the `ftty` terminal emulator domain model.

## Language

**PointerController**:
The stateful controller that maintains pointer interactions, text selection lifecycles, and mouse protocol state.
_Avoid_: MouseManager, PointerHandler, InputService

**PointerOutcome**:
The composite set of semantic actions and compositor cursor updates produced by evaluating a pointer event.
_Avoid_: PointerEvent, MouseAction, InputResult

**Selection**:
A continuous or rectangular span of terminal grid cells marked for clipboard operations and visual highlighting.
_Avoid_: Highlight, Range, TextRange, SelectionRegion

**MouseTracking**:
The terminal mode negotiated by client applications through escape sequences to receive pointer events from the terminal.
_Avoid_: MouseProtocol, XtermMouse, PointerReporting

**GraphicStore**:
The subsystem managing Kitty graphics protocol image decodes, memory budgets, and on-screen placements.
_Avoid_: ImageManager, KittyCache, TextureRegistry

**RenderPipeline**:
The unified rendering system translating terminal cell grids and overlay states into displayable visual output.
_Avoid_: RenderService, DrawingContext, FrameRenderer

**ProceduralGlyphs**:
The procedural geometric generation subsystem synthesizing Unicode Box Drawing and Block Element quads and polygons without font rasterization.
_Avoid_: BoxDrawer, ProceduralRenderer, VectorGlyphs

**GridReflow**:
The column-resizing subsystem extracting logical lines and rewrapping cell matrices across terminal width changes.
_Avoid_: LineRewrapper, GridResizer, TextReflow

**TerminalSession**:
The orchestrator coordinating the pseudo-terminal (PTY) communication channel, escape sequence parsing, and terminal emulation state.
_Avoid_: SessionManager, TermSession, PtyBridge

**SessionOutcome**:
The composite set of platform notifications produced by evaluating terminal PTY input and escape sequences.
_Avoid_: TerminalEvent, SessionUpdate, ProcessResult
