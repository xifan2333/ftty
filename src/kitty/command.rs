//! Kitty graphics protocol control key parameter parser.

use crate::kitty::model::{DeleteTarget, KittyAction, KittyCommand, KittyFormat, KittyMedium};

/// Parses the comma-separated control keys string into a `KittyCommand`.
#[must_use]
pub fn parse_control_keys(s: &str) -> KittyCommand {
    let mut cmd = KittyCommand::default();
    let mut delete_selector = None;

    for pair in s.split(',') {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        let mut parts = pair.splitn(2, '=');
        let key = parts.next().unwrap_or("").trim();
        let val = parts.next().unwrap_or("").trim();

        match key {
            "a" => {
                cmd.action = match val {
                    "t" => KittyAction::TransmitAndDisplay,
                    "T" => KittyAction::TransmitAndDisplayWithResponse,
                    "q" => KittyAction::Query,
                    "p" => KittyAction::Place,
                    "d" => KittyAction::Delete,
                    _ => KittyAction::TransmitAndDisplay,
                };
            }
            "f" => {
                cmd.format = match val {
                    "32" => KittyFormat::Rgba32,
                    "24" => KittyFormat::Rgb24,
                    "100" => KittyFormat::Png,
                    _ => KittyFormat::Rgba32,
                };
            }
            "t" => {
                cmd.medium = match val {
                    "d" => KittyMedium::Direct,
                    "f" => KittyMedium::File,
                    "t" => KittyMedium::TempFile,
                    "s" => KittyMedium::SharedMemory,
                    _ => KittyMedium::Direct,
                };
            }
            "d" => {
                delete_selector = Some(val);
            }
            "s" => cmd.width = val.parse().ok(),
            "v" => cmd.height = val.parse().ok(),
            "i" => {
                cmd.image_id = val.parse().ok();
                cmd.id_explicit = true;
            }
            "p" => cmd.placement_id = val.parse().ok(),
            "c" => cmd.cols = val.parse().ok(),
            "r" => cmd.rows = val.parse().ok(),
            "X" => cmd.offset_x = val.parse().unwrap_or(0),
            "Y" => cmd.offset_y = val.parse().unwrap_or(0),
            "z" => cmd.z_index = val.parse().unwrap_or(0),
            "m" => cmd.more_chunks = val == "1",
            "C" => cmd.do_not_move_cursor = val == "1",
            "U" => cmd.is_virtual = val == "1",
            "q" => cmd.quiet = val.parse().unwrap_or(0),
            _ => {}
        }
    }

    if let Some(val) = delete_selector {
        cmd.delete_target = match val {
            "a" | "A" => DeleteTarget::All,
            "c" | "C" => DeleteTarget::AtCursor,
            "i" | "I" => cmd.image_id.map_or(DeleteTarget::All, DeleteTarget::ById),
            "p" | "P" => cmd
                .placement_id
                .map_or(DeleteTarget::All, DeleteTarget::ByPlacement),
            _ => DeleteTarget::All,
        };
    }

    cmd
}
