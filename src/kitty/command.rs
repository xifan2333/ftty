//! Parses the `key=value,...` control block of one APC command.

use super::model::{
    DeleteTarget, KittyAction, KittyCommand, KittyCompression, KittyFormat, KittyMedium,
};

/// Parses the comma-separated control keys string into a `KittyCommand`.
#[must_use]
pub fn parse_control_keys(s: &str) -> KittyCommand {
    let mut cmd = KittyCommand::default();
    let mut delete_selector: Option<&str> = None;

    fn set_error(cmd: &mut KittyCommand, message: &'static str) {
        if cmd.error.is_none() {
            cmd.error = Some(message);
        }
    }

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
                    "t" => KittyAction::Transmit,
                    "T" => KittyAction::TransmitAndDisplay,
                    "q" => KittyAction::Query,
                    "p" => KittyAction::Place,
                    "d" => KittyAction::Delete,
                    "f" => {
                        set_error(&mut cmd, "ENOTSUP: animation is not supported");
                        KittyAction::TransmitFrame
                    }
                    "a" => {
                        set_error(&mut cmd, "ENOTSUP: animation is not supported");
                        KittyAction::ControlAnimation
                    }
                    "c" => {
                        set_error(&mut cmd, "ENOTSUP: animation is not supported");
                        KittyAction::ComposeFrame
                    }
                    _ => {
                        set_error(&mut cmd, "EINVAL: unknown action");
                        KittyAction::Transmit
                    }
                };
            }
            "f" => {
                cmd.format_explicit = true;
                cmd.format = match val {
                    "32" => KittyFormat::Rgba32,
                    "24" => KittyFormat::Rgb24,
                    "100" => KittyFormat::Png,
                    _ => {
                        set_error(&mut cmd, "EINVAL: unknown image format");
                        KittyFormat::Rgba32
                    }
                };
            }
            "t" => {
                cmd.medium_explicit = true;
                cmd.medium = match val {
                    "d" => KittyMedium::Direct,
                    "f" => KittyMedium::File,
                    "t" => KittyMedium::TempFile,
                    "s" => KittyMedium::SharedMemory,
                    _ => {
                        set_error(&mut cmd, "EINVAL: unknown transmission medium");
                        KittyMedium::Direct
                    }
                };
            }
            "o" => match val {
                "z" => cmd.compression = Some(KittyCompression::Zlib),
                _ => set_error(&mut cmd, "EINVAL: unknown image compression"),
            },
            "d" => delete_selector = Some(val),
            "s" => match val.parse() {
                Ok(0) | Err(_) => {
                    if val.parse::<u32>().is_err() {
                        set_error(&mut cmd, "EINVAL: invalid image width");
                    }
                    cmd.width = None;
                }
                Ok(width) => cmd.width = Some(width),
            },
            "v" => match val.parse() {
                Ok(0) | Err(_) => {
                    if val.parse::<u32>().is_err() {
                        set_error(&mut cmd, "EINVAL: invalid image height");
                    }
                    cmd.height = None;
                }
                Ok(height) => cmd.height = Some(height),
            },
            "S" => match val.parse() {
                Ok(0) => cmd.data_size = None,
                Ok(size) => cmd.data_size = Some(size),
                Err(_) => set_error(&mut cmd, "EINVAL: invalid data size"),
            },
            "O" => match val.parse() {
                Ok(offset) => cmd.data_offset = offset,
                Err(_) => set_error(&mut cmd, "EINVAL: invalid data offset"),
            },
            "i" => {
                cmd.id_explicit = true;
                match val.parse() {
                    Ok(0) | Err(_) => {
                        if val.parse::<u32>().is_err() {
                            set_error(&mut cmd, "EINVAL: invalid image id");
                        }
                        cmd.image_id = None;
                    }
                    Ok(id) => cmd.image_id = Some(id),
                }
            }
            "I" => {
                cmd.number_explicit = true;
                match val.parse() {
                    Ok(0) | Err(_) => {
                        if val.parse::<u32>().is_err() {
                            set_error(&mut cmd, "EINVAL: invalid image number");
                        }
                        cmd.image_number = None;
                    }
                    Ok(number) => cmd.image_number = Some(number),
                }
            }
            "p" => match val.parse() {
                Ok(0) | Err(_) => {
                    if val.parse::<u32>().is_err() {
                        set_error(&mut cmd, "EINVAL: invalid placement id");
                    }
                    cmd.placement_id = None;
                }
                Ok(id) => cmd.placement_id = Some(id),
            },
            "c" => match val.parse() {
                Ok(0) | Err(_) => {
                    if val.parse::<u32>().is_err() {
                        set_error(&mut cmd, "EINVAL: invalid column count");
                    }
                    cmd.cols = None;
                }
                Ok(cols) => cmd.cols = Some(cols),
            },
            "r" => match val.parse() {
                Ok(0) | Err(_) => {
                    if val.parse::<u32>().is_err() {
                        set_error(&mut cmd, "EINVAL: invalid row count");
                    }
                    cmd.rows = None;
                }
                Ok(rows) => cmd.rows = Some(rows),
            },
            "x" => match val.parse() {
                Ok(x) => cmd.src_x = Some(x),
                Err(_) => set_error(&mut cmd, "EINVAL: invalid source x"),
            },
            "y" => match val.parse() {
                Ok(y) => cmd.src_y = Some(y),
                Err(_) => set_error(&mut cmd, "EINVAL: invalid source y"),
            },
            "w" => match val.parse() {
                Ok(0) | Err(_) => {
                    if val.parse::<u32>().is_err() {
                        set_error(&mut cmd, "EINVAL: invalid source width");
                    }
                    cmd.src_w = None;
                }
                Ok(width) => cmd.src_w = Some(width),
            },
            "h" => match val.parse() {
                Ok(0) | Err(_) => {
                    if val.parse::<u32>().is_err() {
                        set_error(&mut cmd, "EINVAL: invalid source height");
                    }
                    cmd.src_h = None;
                }
                Ok(height) => cmd.src_h = Some(height),
            },
            "X" => match val.parse() {
                Ok(offset) => cmd.offset_x = offset,
                Err(_) => set_error(&mut cmd, "EINVAL: invalid x offset"),
            },
            "Y" => match val.parse() {
                Ok(offset) => cmd.offset_y = offset,
                Err(_) => set_error(&mut cmd, "EINVAL: invalid y offset"),
            },
            "z" => match val.parse() {
                Ok(z) => cmd.z_index = z,
                Err(_) => set_error(&mut cmd, "EINVAL: invalid z-index"),
            },
            "P" => {
                match val.parse() {
                    Ok(parent) => cmd.parent_id = Some(parent),
                    Err(_) => set_error(&mut cmd, "EINVAL: invalid parent image id"),
                }
                set_error(&mut cmd, "ENOTSUP: relative placements are not supported");
            }
            "m" => match val {
                "0" => cmd.more_chunks = false,
                "1" => cmd.more_chunks = true,
                _ => set_error(&mut cmd, "EINVAL: invalid chunk flag"),
            },
            "C" => match val {
                "0" => cmd.do_not_move_cursor = false,
                "1" => cmd.do_not_move_cursor = true,
                _ => set_error(&mut cmd, "EINVAL: invalid cursor flag"),
            },
            "U" => match val {
                "0" => cmd.is_virtual = false,
                "1" => cmd.is_virtual = true,
                _ => set_error(&mut cmd, "EINVAL: invalid virtual placement flag"),
            },
            "q" => match val {
                "0" => cmd.quiet = 0,
                "1" => cmd.quiet = 1,
                "2" => cmd.quiet = 2,
                _ => set_error(&mut cmd, "EINVAL: invalid quiet level"),
            },
            // Usage hints are advisory; ftty does not need to act on them.
            "N" => {}
            _ => {}
        }
    }

    if cmd.image_id.is_some() && cmd.image_number.is_some() {
        set_error(&mut cmd, "EINVAL: both image id and image number specified");
    }

    if let Some(val) = delete_selector {
        let x = cmd.src_x.unwrap_or(0);
        let y = cmd.src_y.unwrap_or(0);
        cmd.delete_target = match val {
            "a" => DeleteTarget::All,
            "A" => DeleteTarget::AllAndFree,
            "c" => DeleteTarget::AtCursor,
            "C" => DeleteTarget::AtCursorAndFree,
            "i" => match (cmd.image_id, cmd.placement_id) {
                (Some(id), Some(placement)) => DeleteTarget::ByIdPlacement(id, placement),
                (Some(id), None) => DeleteTarget::ById(id),
                (None, _) => {
                    set_error(&mut cmd, "EINVAL: image id required");
                    DeleteTarget::All
                }
            },
            "I" => match (cmd.image_id, cmd.placement_id) {
                (Some(id), Some(placement)) => DeleteTarget::ByIdPlacementAndFree(id, placement),
                (Some(id), None) => DeleteTarget::ByIdAndFree(id),
                (None, _) => {
                    set_error(&mut cmd, "EINVAL: image id required");
                    DeleteTarget::AllAndFree
                }
            },
            "n" | "N" => match cmd.image_number {
                Some(number) => DeleteTarget::ByNumber {
                    number,
                    placement: cmd.placement_id,
                    free: val == "N",
                },
                None => {
                    set_error(&mut cmd, "EINVAL: image number required");
                    DeleteTarget::All
                }
            },
            "p" => DeleteTarget::AtAbsoluteCell {
                x,
                y,
                z: None,
                free: false,
            },
            "P" => DeleteTarget::AtAbsoluteCell {
                x,
                y,
                z: None,
                free: true,
            },
            "q" => DeleteTarget::AtAbsoluteCell {
                x,
                y,
                z: Some(cmd.z_index),
                free: false,
            },
            "Q" => DeleteTarget::AtAbsoluteCell {
                x,
                y,
                z: Some(cmd.z_index),
                free: true,
            },
            "r" => DeleteTarget::ByIdRange {
                lo: x,
                hi: y,
                free: false,
            },
            "R" => DeleteTarget::ByIdRange {
                lo: x,
                hi: y,
                free: true,
            },
            "x" => DeleteTarget::ByColumn { x, free: false },
            "X" => DeleteTarget::ByColumn { x, free: true },
            "y" => DeleteTarget::ByRow { y, free: false },
            "Y" => DeleteTarget::ByRow { y, free: true },
            "z" => DeleteTarget::ByZIndex(cmd.z_index),
            "Z" => DeleteTarget::ByZIndexAndFree(cmd.z_index),
            "f" | "F" => {
                set_error(&mut cmd, "ENOTSUP: animation is not supported");
                DeleteTarget::All
            }
            _ => {
                set_error(&mut cmd, "ENOTSUP: unsupported delete selector");
                DeleteTarget::All
            }
        };
    } else {
        cmd.delete_target = DeleteTarget::All;
    }

    cmd
}
