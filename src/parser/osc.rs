//! Operating System Command (OSC) sequence dispatch and URI parsing.

use std::path::PathBuf;

use base64::Engine;
use base64::prelude::BASE64_STANDARD;

use crate::parser::{ProgressState, ShellIntegrationState, Terminal};

/// Parses an OSC 7 file URI (e.g. `file://localhost/home/user` or `file:///tmp`) into a local PathBuf.
#[must_use]
pub fn parse_osc7_path(raw: &str) -> Option<PathBuf> {
    let stripped = raw.strip_prefix("file://")?;
    let path_str = if let Some(idx) = stripped.find('/') {
        &stripped[idx..]
    } else {
        stripped
    };
    let decoded = percent_decode(path_str)?;
    Some(PathBuf::from(decoded))
}

/// Simple percent-decoding for URL encoded paths.
#[must_use]
pub fn percent_decode(s: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(s.len());
    let mut chars = s.bytes();
    while let Some(b) = chars.next() {
        if b == b'%' {
            let h1 = chars.next()?;
            let h2 = chars.next()?;
            let hex_bytes = [h1, h2];
            let hex_str = std::str::from_utf8(&hex_bytes).ok()?;
            let byte = u8::from_str_radix(hex_str, 16).ok()?;
            bytes.push(byte);
        } else {
            bytes.push(b);
        }
    }
    String::from_utf8(bytes).ok()
}

impl Terminal {
    pub(crate) fn handle_osc(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        if params.len() >= 2
            && (params[0] == b"0" || params[0] == b"2")
            && let Ok(title) = std::str::from_utf8(params[1])
        {
            self.title = title.to_string();
        } else if params.len() >= 2 {
            if params[0] == b"10" && params[1] == b"?" {
                let fg = self.default_fg;
                let resp = format!(
                    "\x1b]10;rgb:{:02x}{:02x}/{:02x}{:02x}/{:02x}{:02x}\x1b\\",
                    fg.r, fg.r, fg.g, fg.g, fg.b, fg.b
                );
                self.responses.push(resp.into_bytes());
            } else if params[0] == b"11" && params[1] == b"?" {
                let bg = self.default_bg;
                let resp = format!(
                    "\x1b]11;rgb:{:02x}{:02x}/{:02x}{:02x}/{:02x}{:02x}\x1b\\",
                    bg.r, bg.r, bg.g, bg.g, bg.b, bg.b
                );
                self.responses.push(resp.into_bytes());
            } else if params[0] == b"52" {
                // OSC 52 ; [Pc] ; Pd
                let (target, payload) = if params.len() == 2 {
                    (b"c".as_slice(), params[1])
                } else {
                    (params[1], params[2])
                };

                let target_str = std::str::from_utf8(target).unwrap_or("c");
                let primary_target = target_str.chars().next().unwrap_or('c');

                if payload == b"?" {
                    let b64 = self
                        .clipboard_content
                        .as_ref()
                        .map(|text| BASE64_STANDARD.encode(text.as_bytes()))
                        .unwrap_or_default();
                    let resp = format!("\x1b]52;{};{}\x1b\\", primary_target, b64);
                    self.responses.push(resp.into_bytes());
                } else if payload.is_empty() {
                    self.clipboard_content = None;
                    self.pending_clipboard = Some(None);
                } else if let Ok(decoded) = BASE64_STANDARD.decode(payload)
                    && let Ok(text) = String::from_utf8(decoded)
                {
                    self.clipboard_content = Some(text.clone());
                    self.pending_clipboard = Some(Some(text));
                }
            } else if params[0] == b"7" && params.len() >= 2 {
                // OSC 7 ; file://[hostname]/path
                if let Ok(uri) = std::str::from_utf8(params[1])
                    && let Some(path) = parse_osc7_path(uri)
                {
                    self.current_dir = Some(path);
                }
            } else if params[0] == b"133" && params.len() >= 2 {
                // OSC 133 ; [A|B|C|D] [; exit_code]
                match params[1] {
                    b"A" => self.shell_integration = Some(ShellIntegrationState::PromptStart),
                    b"B" => self.shell_integration = Some(ShellIntegrationState::CommandStart),
                    b"C" => self.shell_integration = Some(ShellIntegrationState::OutputStart),
                    b"D" => {
                        let exit_code = if params.len() >= 3 {
                            std::str::from_utf8(params[2])
                                .ok()
                                .and_then(|s| s.parse::<i32>().ok())
                        } else {
                            None
                        };
                        self.shell_integration =
                            Some(ShellIntegrationState::CommandFinished(exit_code));
                    }
                    _ => {}
                }
            } else if params[0] == b"9" && params.len() >= 3 && params[1] == b"4" {
                // OSC 9 ; 4 ; state [; progress]
                let state_val = std::str::from_utf8(params[2])
                    .ok()
                    .and_then(|s| s.parse::<u8>().ok());
                let progress_val = if params.len() >= 4 {
                    std::str::from_utf8(params[3])
                        .ok()
                        .and_then(|s| s.parse::<u8>().ok())
                        .unwrap_or(0)
                        .min(100)
                } else {
                    0
                };
                self.progress = match state_val {
                    Some(0) => None,
                    Some(1) => Some(ProgressState::Normal(progress_val)),
                    Some(2) => Some(ProgressState::Error(progress_val)),
                    Some(3) => Some(ProgressState::Indeterminate),
                    Some(4) => Some(ProgressState::Warning(progress_val)),
                    _ => None,
                };
            } else if params[0] == b"8" {
                // OSC 8 ; [params] ; [url]
                let url_bytes = if params.len() >= 3 {
                    params[2]
                } else if params.len() == 2 {
                    params[1]
                } else {
                    b"".as_slice()
                };

                if url_bytes.is_empty() {
                    self.active_hyperlink = None;
                } else if let Ok(url) = std::str::from_utf8(url_bytes) {
                    let id = self.get_or_intern_hyperlink(url.to_string());
                    self.active_hyperlink = if id > 0 { Some(id) } else { None };
                }
            }
        }
    }
}
