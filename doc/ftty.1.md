---
title: FTTY
section: 1
header: ftty Manual
---

# NAME

ftty - Ultra-lightweight minimalist Wayland terminal emulator with native Kitty graphics protocol

# SYNOPSIS

**ftty** [_options_] [**-e** _command_ [_args..._]]

# DESCRIPTION

**ftty** is an ultra-lightweight, minimalist Wayland terminal emulator written in Rust.

It adheres strictly to the Unix philosophy:

- **Do One Thing Well**: **ftty** is strictly a PTY terminal surface. It provides no built-in tabs, split panes, or session multiplexing. Window tiling belongs to the window manager (**xrwm(1)**, **river(1)**), and session multiplexing belongs to multiplexers like **herdr(1)** or **tmux(1)**.
- **Minimalist & Bloat-Free**: Implemented directly as a native Wayland client using EGL and OpenGL ES without heavy GUI toolkits (GTK, Qt, GPUI).
- **Core Protocol Support**: Built-in native Kitty graphics protocol (APC `\x1b_G`) with zero-copy shared memory transfers for terminal tools such as **yazi(1)** and **image.nvim**, first-class **fcitx5(1)** IME integration via Wayland `text-input-v3`, and signal-driven zero-flicker dynamic palette switching.

When executed without arguments, **ftty** starts the user's default shell determined by the `$SHELL` environment variable (or `/bin/sh` if unset).

# OPTIONS

**-a**, **\-\-app\-id** _id_
: Sets the Wayland surface application ID (app-id). Defaults to `ftty`. This identifier is used by Wayland compositors and window managers for matching window rules and layout assignment.

**-T**, **\-\-title** _title_
: Sets the initial window title. Defaults to `ftty`. Applications running inside the terminal may dynamically update the title via standard OSC 0 and OSC 2 escape sequences.

**-d**, **\-\-working\-directory** _path_
: Specifies the initial working directory for the child process. If the directory does not exist, **ftty** exits with an error.

**-c**, **\-\-config** _path_
: Specifies an explicit path to a TOML configuration file. If omitted, **ftty** attempts to load configuration from `$XDG_CONFIG_HOME/ftty/ftty.toml` or `~/.config/ftty/ftty.toml`. If the default file does not exist, built-in defaults are used.

**-e** _command_ [_args..._]
: Executes the specified _command_ with subsequent arguments directly instead of launching the default login shell. Must be passed as the last option on the command line.

**-h**, **\-\-help**
: Prints a summary of command-line usage and options, then exits.

**-v**, **\-\-version**
: Prints the **ftty** version number and exits.

# CONFIGURATION

**ftty** uses declarative TOML for configuration. The configuration file supports modular composition via `include` directives.

## INCLUDES

**include** = [*"path/to/theme.toml"*, ...]
: An array of file paths to merge into the current configuration. Paths may be absolute or relative to the directory of the file containing the include. Merged files are evaluated in order, with options in the root file taking precedence over included values. Circular includes are automatically detected and prevented.

## FONT SECTION

The `[font]` table configures typography and glyph rendering:

**family** = *"monospace"*
: Specifies a single font family name, or an ordered array of fallback font families:

```toml
[font]
family = ["JetBrains Mono", "Noto Color Emoji", "monospace"]
```

*(The alias `families` is also accepted).*

**size** = *9.0*
: Font point size as a floating-point number. Defaults to `9.0`.

## WINDOW SECTION

The `[window]` table configures surface metrics:

**padding** = *4* | [*4*, *2*]
: Internal surface padding in logical pixels between the terminal grid and window borders. May be specified as a uniform scalar (`padding = 4`) or as a two-element array `[horizontal, vertical]` (`padding = [4, 2]`). Defaults to `4`.

## CURSOR SECTION

The `[cursor]` table configures cursor styling:

**shape** = *"block"* | *"beam"* | *"underline"*
: Sets the initial cursor shape. Defaults to `"block"`.

## COLORS SECTION

The `[colors]` table defines the color palette:

**foreground** = *"#dcdcdc"*
: Primary text foreground color in hexadecimal (`#RRGGBB` or `0xRRGGBB`).

**background** = *"#181818"*
: Surface background color in hexadecimal.

Standard 16 ANSI colors:

- **black**, **red**, **green**, **yellow**, **blue**, **magenta**, **cyan**, **white**
- **bright_black**, **bright_red**, **bright_green**, **bright_yellow**, **bright_blue**, **bright_magenta**, **bright_cyan**, **bright_white**

### Extended 256-Color Palette

The `[colors.palette]` subtable allows overriding any of the 256 indexed ANSI colors:

```toml
[colors.palette]
"16" = "#1a1b26"
"240" = "#414868"
```

## SCROLLBACK SECTION

The `[scrollback]` table configures the terminal history buffer and scrolling behavior:

**lines** = *10000*
: Maximum number of lines retained in the scrollback buffer. Defaults to `10000`.

**multiplier** = *3.0*
: Scroll distance multiplier for mouse wheel and touchpad events. Defaults to `3.0`.

**auto_scroll** = *true*
: Automatically scrolls the viewport back to the bottom when new output is received from the running process or a key is pressed. Defaults to `true`.

## CLIPBOARD SECTION

The `[clipboard]` table controls security policies for OSC 52 clipboard access:

**allow_osc52_read** = *false*
: Governs whether CLI programs are permitted to query and read the Wayland system clipboard via OSC 52 escape sequences. Defaults to `false` as a security defense to prevent untrusted remote processes (e.g. over SSH) from silently exfiltrating clipboard content.

**allow_osc52_write** = *true*
: Governs whether CLI programs (such as **nvim(1)**, **tmux(1)**, or **yazi(1)**) are permitted to update or clear the system clipboard via OSC 52. Defaults to `true`.

## KEYBINDINGS SECTION

The `[keybindings]` table maps keyboard shortcuts to terminal actions, external pipelines, or unbinds defaults.

### Mapping Syntax

Keybindings support bidirectional mapping and unbinding:

1. **Key combination to action**:
```toml
[keybindings]
"Ctrl+Shift+C" = "clipboard_copy"
"Ctrl+Shift+V" = "clipboard_paste"
```

2. **Action to key or array of keys**:
```toml
[keybindings]
scrollback_up_page = "Shift+Page_Up"
font_increase = ["Ctrl+plus", "Ctrl+equal"]
```

3. **Unbinding defaults**:
```toml
[keybindings]
"Ctrl+Shift+V" = "none"
```

### Standard Built-In Actions

**clipboard_copy**
: Copies the active text selection to the Wayland system clipboard.

**clipboard_paste**
: Pastes current text from the Wayland system clipboard to the terminal PTY.

**primary_paste**
: Pastes text from the Wayland primary selection buffer to the terminal PTY.

**font_increase**
: Increases the current font size dynamically by 1.0pt.

**font_decrease**
: Decreases the current font size dynamically by 1.0pt (bounded by minimum point size).

**font_reset**
: Resets the font size to the value configured in the `[font]` section.

**scrollback_up_page**
: Scrolls the viewport upward by one visible page.

**scrollback_down_page**
: Scrolls the viewport downward by one visible page.

**scrollback_up_line**
: Scrolls the viewport upward by a single line.

**scrollback_down_line**
: Scrolls the viewport downward by a single line.

**scrollback_home**
: Scrolls directly to the oldest retained line in the scrollback buffer.

**scrollback_end**
: Scrolls directly to the bottom of the active screen.

**prompt_prev**
: Jumps viewport upward to the previous shell prompt using semantic prompt markers.

**prompt_next**
: Jumps viewport downward to the next shell prompt using semantic prompt markers.

### Pipe Actions (Asynchronous External Pipelines)

**ftty** supports streaming terminal buffer text into external commands via standard input without blocking the main event loop:

**pipe_visible**
: Extracts the text currently displayed in the visible viewport and streams it to the stdin of the specified command:
```toml
[keybindings]
"Ctrl+Shift+U" = { pipe_visible = ["urlscan"] }
```

**pipe_scrollback**
: Extracts the entire scrollback history buffer and streams it to the stdin of the command:
```toml
[keybindings]
"Ctrl+Shift+F" = { pipe_scrollback = ["sh", "-c", "fzf | wl-copy"] }
```

**pipe_selection**
: Extracts the currently selected text and streams it to the stdin of the command:
```toml
[keybindings]
"Ctrl+Shift+Y" = { pipe_selection = "wl-copy" }
```

*Note: Commands may be given as a parameter list (executed directly) or as a single command string (executed via `sh -c`).*

### Default Keybindings

Unless overridden in `[keybindings]`, the following bindings are active by default:

- **Ctrl+Shift+C**, **Ctrl+Insert**: `clipboard_copy`
- **Ctrl+Shift+V**, **Shift+Insert**: `clipboard_paste`
- **Ctrl+plus**, **Ctrl+equal**: `font_increase`
- **Ctrl+minus**: `font_decrease`
- **Ctrl+0**: `font_reset`
- **Shift+Page_Up**, **Shift+KP_Page_Up**: `scrollback_up_page`
- **Shift+Page_Down**, **Shift+KP_Page_Down**: `scrollback_down_page`
- **Ctrl+Shift+Up**: `scrollback_up_line`
- **Ctrl+Shift+Down**: `scrollback_down_line`
- **Shift+Home**: `scrollback_home`
- **Shift+End**: `scrollback_end`
- **Ctrl+Shift+Z**: `prompt_prev`
- **Ctrl+Shift+X**: `prompt_next`

# SIGNALS

**SIGUSR1**
: Triggers an immediate, zero-flicker dynamic configuration and palette reload. The configuration file and all included palettes are reparsed, and GL texture atlases and font faces are dynamically refreshed without restarting the terminal or interrupting child processes.

**SIGWINCH**
: Sent automatically to the child PTY foreground process group when the Wayland surface resizes or font dimensions change.

# PROTOCOLS & EXTENSIONS

**Kitty Graphics Protocol**
: Implements native Kitty graphics escape codes (`\x1b_G...;payload\x1b\`). Supports action types transmit (`a=t`), transmit and display (`a=T`), query (`a=q`), delete (`a=d`), and placement (`a=p`). Zero-copy shared memory (`t=s`, `t=f`) and direct transmission (`t=d`, `t=m`) modes are supported, allowing seamless, high-performance image display in tools like **yazi(1)** and **image.nvim**. Responses are formatted strictly in compliance with the Kitty specification.

**Wayland text-input-v3 (IME)**
: Fully integrates Wayland `zwp_text_input_v3` protocol for input method engines such as **fcitx5(1)**. Accurately reports cursor rectangle coordinates in surface space to position candidate popup windows, with native preedit text rendering and styling.

**Kitty Keyboard Protocol**
: Supports progressive keyboard enhancement modes (disambiguate escape codes, report alternate keys, report all keys as escape sequences) to eliminate legacy terminal escape ambiguities.

**OSC 52 Clipboard**
: Supports reading and writing the system clipboard via standard OSC 52 base64 escape sequences.

# FILES

`$XDG_CONFIG_HOME/ftty/ftty.toml` or `~/.config/ftty/ftty.toml`
: Default user configuration file location.

# ENVIRONMENT

**FTTY_CONFIG**
: Optional path to an alternative configuration file.

**TERM**
: Terminal type identifier exported to child processes. Set to `xterm-256color`.

**COLORTERM**
: Set to `truecolor` to announce 24-bit direct RGB color capability.

**WAYLAND_DISPLAY**
: Names the Wayland compositor socket used for connection.

# AUTHORS

Developed by xifan2333 (<xifan233@163.com>) and ftty contributors.

# SEE ALSO

**xrwm(1)**, **herdr(1)**, **kitty(1)**, **yazi(1)**, **fcitx5(1)**
