# Research: Terminal Plaintext URL Detection and DeepLink Scheme Resolution

## 1. Executive Summary

In terminal emulators, URL detection transforms plain terminal text into interactive, clickable hyperlinks (typically invoked via `Ctrl+Click` or keyboard jump-labels).

The current implementation in `ftty` (`src/input/pointer.rs:find_url_in_grid`) exhibits two critical flaws:
1. **Unbounded Scan & Absorption of Adjacent Content**: The URL termination check relies on a crude character blacklist (`ch.is_whitespace() || ch == '<' ...`). Because non-ASCII characters (such as CJK text, full-width punctuation, and emojis) are not in this blacklist, any text immediately following a URL (e.g. `https://github.com/foo这是中文说明`) is absorbed into the link target.
2. **Hardcoded Scheme Enumeration**: The scanner only recognizes a static slice (`["https://", "http://", "file://", "gemini://"]`), completely failing to recognize application DeepLinks (e.g. `vscode://`, `cursor://`, `idea://`, `obsidian://`, `git://`, `docker://`).

This document surveys high-trust primary sources (IETF RFC specifications and production implementations in Alacritty, Ghostty, Foot, and Contour) to synthesize a standard, bloat-free solution tailored for `ftty`.

---

## 2. Primary Source Investigations

### 2.1 IETF RFC 3986: Uniform Resource Identifier (URI) Generic Syntax

- **Source**: [IETF RFC 3986 (STD 66)](https://www.rfc-editor.org/rfc/rfc3986)
- **Key Findings**:

1. **Character Set (§2, §2.1 - §2.3)**:
   A valid URI consists exclusively of a subset of the US-ASCII character set:
   - `unreserved` = `ALPHA / DIGIT / "-" / "." / "_" / "~"` (§2.3)
   - `gen-delims` = `":" / "/" / "?" / "#" / "[" / "]" / "@"` (§2.2)
   - `sub-delims` = `"!" / "$" / "&" / "'" / "(" / ")" / "*" / "+" / "," / ";" / "="` (§2.2)
   - `pct-encoded` = `"%" HEXDIG HEXDIG` (§2.1)
   
   **Critical Implication**: Any non-ASCII character (including Unicode code points `> U+007F`, CJK ideographs `U+4E00..U+9FFF`, full-width punctuation like `，` `U+FF0C`, and emojis) is **never** a valid raw character in a URI. A compliant URL scanner must terminate immediately upon encountering any non-ASCII character or whitespace.

2. **Scheme Syntax (§3.1)**:
   ```abnf
   scheme = ALPHA *( ALPHA / DIGIT / "+" / "-" / "." )
   ```
   For hierarchical URIs with an authority (network or application protocols), the scheme is delimited from the rest of the URI by the sequence `://`.
   
   **Critical Implication**: Any protocol conforming to URI standards starts with an ASCII letter, contains only ASCII alphanumerics, `+`, `-`, or `.`, and connects to the authority component via `://`. Rather than enumerating protocol names, looking for the `://` delimiter and validating the scheme backward naturally covers all hierarchical URIs and DeepLinks.

---

### 2.2 Ghostty Terminal

- **Source**: [ghostty-org/ghostty: `src/config/url.zig`](https://github.com/ghostty-org/ghostty/blob/main/src/config/url.zig)
- **Key Findings**:
  Ghostty's built-in URL scanner employs a positive character whitelist derived from RFC 3986:
  ```zig
  const scheme_url_chars = \\[\w\-.~:/?#@!$&*+,;=%]
  ```
  It defines strict heuristics for trailing sentence punctuation and surrounding brackets:
  1. Trailing punctuation (`.` and `,`) is excluded via negative lookbehind.
  2. Balanced parentheses heuristic: Parentheses `)` are excluded unless there is an unmatched opening `(` earlier in the URL (to accommodate Wikipedia URLs such as `https://en.wikipedia.org/wiki/Rust_(programming_language)`).

---

### 2.3 Alacritty Terminal

- **Source**:
  - `alacritty/src/config/ui_config.rs` (`URL_REGEX`)
  - `alacritty/src/display/hint.rs` (`HintPostProcessor::hint_post_processing`)
- **Key Findings**:
  Alacritty applies a two-stage approach for plaintext URL discovery:
  1. **Pattern Matching**: Matches candidate URL strings starting with supported schemes or scheme patterns.
  2. **Heuristic Post-Processing (`HintPostProcessor`)**:
     - **Bracket Balancing**: Scans forward counting `(` vs `)` and `[` vs `]`. If an unmatched closing bracket is encountered at the end, the match boundary is retracted to exclude it.
     - **Trailing Delimiter Trimming**: Retracts the match boundary over trailing punctuation:
       ```rust
       matches!(c, '.' | ',' | ':' | ';' | '?' | '!' | '(' | '[' | '\'')
       ```
     - This guarantees that sentences like `See https://example.com/api.` do not pass `api.` to the browser.

---

### 2.4 Foot Terminal Emulator

- **Source**:
  - `dnkl/foot: url-mode.c`, `uri.c`
  - Foot Issue #674 (`url.uri-characters`) & Issue #1935 (`Generic, regex-based label mode`)
- **Key Findings**:
  - Foot originally bounded URL extraction using an explicit `uri-characters` set matching RFC 3986 unreserved + reserved characters.
  - Foot establishes that characters outside the URI set constitute URL boundaries. This ensures that non-ASCII characters in East Asian multilingual text immediately delimit the URL without bleeding into adjacent prose.

---

## 3. Diagnosis of Current `ftty` Defect

In `ftty`, `find_url_in_grid` in `src/input/pointer.rs` currently reads:

```rust
let schemes = ["https://", "http://", "file://", "gemini://"];
for scheme in &schemes {
    let mut search_from = 0;
    while let Some(pos) = text[search_from..].find(scheme) {
        let start = search_from + pos;
        let mut end = start;
        for (idx, ch) in text[start..].char_indices() {
            if ch.is_whitespace()
                || ch == '<'
                || ch == '>'
                || ch == '"'
                || ch == '`'
                || ch == '^'
                || ch == '\\'
                || ch == '|'
            {
                break;
            }
            end = start + idx + ch.len_utf8();
        }

        while end > start {
            let Some(last_char) = text[..end].chars().next_back() else {
                break;
            };
            if matches!(
                last_char,
                '.' | ',' | '!' | '?' | ';' | ':' | ')' | ']' | '}' | '\'' | '"'
            ) {
                if last_char == ')'
                    && text[start..end].matches('(').count()
                        == text[start..end].matches(')').count()
                {
                    break;
                }
                end -= last_char.len_utf8();
            } else {
                break;
            }
        }
```

### Breakdown of Root Causes:

1. **Negative Blacklist Fallacy**:
   - The loop checks `if ch.is_whitespace() || ch == '<' ... break;`.
   - Any character not explicitly in those 8 entries is accepted as valid URL content.
   - When text contains `https://example.com/repo这是仓库说明` or `https://example.com/api，请查阅`, all Chinese characters (`这是仓库说明`) and full-width punctuation (`，`) pass through unhindered.
   - The resulting URL passed to `xdg-open` and the hovered underline span extends over the subsequent text.

2. **Hardcoded Schemes Slice**:
   - `["https://", "http://", "file://", "gemini://"]` is a hardcoded list.
   - Any DeepLink (`vscode://...`, `cursor://...`, `idea://...`, `obsidian://...`, `git://...`, `docker://...`) is ignored, causing `find_url_in_grid` to return `None` and the mouse click to fall through to normal text selection.

---

## 4. Proposed Solution Architecture for `ftty`

In adherence to `ftty`'s **Supreme Architecture Principles** (minimalist, bloat-free, Unix philosophy, zero heavy dependencies), we replace the flawed blacklist with a standard RFC 3986 state-machine scanner.

### 4.1 RFC 3986 Character Whitelist Validator

Define a pure, zero-allocation inline helper:

```rust
#[inline]
fn is_url_char(c: char) -> bool {
    // RFC 3986: unreserved, gen-delims, and sub-delims
    c.is_ascii_alphanumeric() || matches!(
        c,
        '-' | '.' | '_' | '~' | ':' | '/' | '?' | '#' | '[' | ']' | '@'
            | '!' | '$' | '&' | '\'' | '(' | ')' | '*' | '+' | ',' | ';' | '=' | '%'
    )
}
```

Any non-ASCII character (including CJK characters, full-width punctuation `，` `。` `！`, and emojis) or whitespace returns `false` and terminates the URL forward scan immediately.

### 4.2 Dynamic `://` Anchor & Backward Scheme Extraction

Instead of looping over a fixed list of schemes:
1. Search `text` for the anchor `"://"`.
2. Given `pos` as the byte index of `"://"`, scan backward to extract the scheme:
   - According to RFC 3986 §3.1, scheme characters must satisfy:
     `c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.'`.
   - The scheme must begin with an ASCII alphabetic character: `c.is_ascii_alphabetic()`.
   - Scheme length is bounded (typically `2..=32`).
3. This dynamically matches:
   - Standard web schemes: `http`, `https`, `file`, `gemini`, `ftp`.
   - Developer deep links: `vscode`, `cursor`, `idea`, `obsidian`, `git`, `ssh`, `docker`, `man`, etc.

### 4.3 Forward Scanning with Authority Verification

From `pos + 3` (immediately after `://`):
1. Verify that at least one valid host character follows `://` (avoid matching bare `http://` in prose).
2. Scan forward while `is_url_char(c)` is true.

### 4.4 Balanced Bracket and Trailing Punctuation Retraction

Apply two-stage trimming on the extracted candidate:
1. **Unbalanced Closing Brackets**:
   - If candidate ends with `)` and count of `)` > count of `(`, strip trailing `)`.
   - If candidate ends with `]` and count of `]` > count of `[`, strip trailing `]`.
   - If candidate ends with `}` and count of `}` > count of `{`, strip trailing `}`.
2. **Trailing Sentence Punctuation**:
   - Strip any trailing `.` `,` `;` `:` `!` `?` `'` `"`.
   - Repeat until no trailing punctuation or unbalanced brackets remain.

### 4.5 Exact Grid Coordinate Mapping

Preserve the existing `coords: Vec<(abs_line, col, width)>` indexing so that:
- Character count offsets `start_char..end_char` map back to physical grid cells `(min_col, max_col)`.
- Wrapped lines are correctly joined and highlighted without coordinate drift.

---

## 5. Verification Plan

1. **Unit Tests in `src/input/pointer.rs`**:
   - Test pure URL followed by CJK text: `https://github.com/foo这是中文` -> extracts exactly `https://github.com/foo`.
   - Test pure URL followed by full-width punctuation: `https://github.com/foo，请查阅` -> extracts exactly `https://github.com/foo`.
   - Test DeepLink schemes:
     - `vscode://file/home/user/project/main.rs:10`
     - `cursor://file/path`
     - `obsidian://open?vault=work`
     - `docker://container/id`
     - `git://github.com/user/repo.git`
   - Test trailing sentence punctuation: `Visit https://example.com.` -> `https://example.com`.
   - Test Wikipedia parenthesized URL: `https://en.wikipedia.org/wiki/Rust_(programming_language)` -> preserves full URL including parentheses.
   - Test markdown wrapped URL: `[https://example.com]` -> `https://example.com`.
   - Test wrapped URL across lines.

2. **Quality Gates**:
   - `mise run check:changed` passes with zero warnings.
   - All tests pass with < 2s runtime.
