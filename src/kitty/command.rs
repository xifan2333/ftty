//! Parses the `key=value,...` control block of one APC command.

use super::model::{
    DeleteTarget, KittyAction, KittyCommand, KittyCompression, KittyFormat, KittyMedium,
};

fn set_error(cmd: &mut KittyCommand, message: &'static str) {
    if cmd.error.is_none() {
        cmd.error = Some(message);
    }
}

fn parse_optional_non_zero_u32(
    cmd: &mut KittyCommand,
    val: &str,
    err_msg: &'static str,
) -> Option<u32> {
    match val.parse() {
        Ok(0) => None,
        Ok(n) => Some(n),
        Err(_) => {
            set_error(cmd, err_msg);
            None
        }
    }
}

fn parse_action(cmd: &mut KittyCommand, val: &str) -> KittyAction {
    match val {
        "t" => KittyAction::Transmit,
        "T" => KittyAction::TransmitAndDisplay,
        "q" => KittyAction::Query,
        "p" => KittyAction::Place,
        "d" => KittyAction::Delete,
        "f" => {
            set_error(cmd, "ENOTSUP: animation is not supported");
            KittyAction::TransmitFrame
        }
        "a" => {
            set_error(cmd, "ENOTSUP: animation is not supported");
            KittyAction::ControlAnimation
        }
        "c" => {
            set_error(cmd, "ENOTSUP: animation is not supported");
            KittyAction::ComposeFrame
        }
        _ => {
            set_error(cmd, "EINVAL: unknown action");
            KittyAction::Transmit
        }
    }
}

fn parse_format(cmd: &mut KittyCommand, val: &str) -> KittyFormat {
    match val {
        "32" => KittyFormat::Rgba32,
        "24" => KittyFormat::Rgb24,
        "100" => KittyFormat::Png,
        _ => {
            set_error(cmd, "EINVAL: unknown image format");
            KittyFormat::Rgba32
        }
    }
}

fn parse_medium(cmd: &mut KittyCommand, val: &str) -> KittyMedium {
    match val {
        "d" => KittyMedium::Direct,
        "f" => KittyMedium::File,
        "t" => KittyMedium::TempFile,
        "s" => KittyMedium::SharedMemory,
        _ => {
            set_error(cmd, "EINVAL: unknown transmission medium");
            KittyMedium::Direct
        }
    }
}

fn parse_transmission_key<'a>(
    cmd: &mut KittyCommand,
    key: &str,
    val: &'a str,
    delete_selector: &mut Option<&'a str>,
) -> bool {
    match key {
        "a" => {
            cmd.action = parse_action(cmd, val);
            true
        }
        "f" => {
            cmd.format_explicit = true;
            cmd.format = parse_format(cmd, val);
            true
        }
        "t" => {
            cmd.medium_explicit = true;
            cmd.medium = parse_medium(cmd, val);
            true
        }
        "o" => {
            match val {
                "z" => cmd.compression = Some(KittyCompression::Zlib),
                _ => set_error(cmd, "EINVAL: unknown image compression"),
            }
            true
        }
        "d" => {
            *delete_selector = Some(val);
            true
        }
        "m" => {
            match val {
                "0" => cmd.more_chunks = false,
                "1" => cmd.more_chunks = true,
                _ => set_error(cmd, "EINVAL: invalid chunk flag"),
            }
            true
        }
        "q" => {
            match val {
                "0" => cmd.quiet = 0,
                "1" => cmd.quiet = 1,
                "2" => cmd.quiet = 2,
                _ => set_error(cmd, "EINVAL: invalid quiet level"),
            }
            true
        }
        _ => false,
    }
}

fn parse_dimension_key(cmd: &mut KittyCommand, key: &str, val: &str) -> bool {
    match key {
        "s" => {
            cmd.width = parse_optional_non_zero_u32(cmd, val, "EINVAL: invalid image width");
            true
        }
        "v" => {
            cmd.height = parse_optional_non_zero_u32(cmd, val, "EINVAL: invalid image height");
            true
        }
        "c" => {
            cmd.cols = parse_optional_non_zero_u32(cmd, val, "EINVAL: invalid column count");
            true
        }
        "r" => {
            cmd.rows = parse_optional_non_zero_u32(cmd, val, "EINVAL: invalid row count");
            true
        }
        "w" => {
            cmd.src_w = parse_optional_non_zero_u32(cmd, val, "EINVAL: invalid source width");
            true
        }
        "h" => {
            cmd.src_h = parse_optional_non_zero_u32(cmd, val, "EINVAL: invalid source height");
            true
        }
        _ => false,
    }
}

fn parse_placement_coordinate_key(cmd: &mut KittyCommand, key: &str, val: &str) -> bool {
    match key {
        "x" => {
            match val.parse() {
                Ok(x) => cmd.src_x = Some(x),
                Err(_) => set_error(cmd, "EINVAL: invalid source x"),
            }
            true
        }
        "y" => {
            match val.parse() {
                Ok(y) => cmd.src_y = Some(y),
                Err(_) => set_error(cmd, "EINVAL: invalid source y"),
            }
            true
        }
        "X" => {
            match val.parse() {
                Ok(offset) => cmd.offset_x = offset,
                Err(_) => set_error(cmd, "EINVAL: invalid x offset"),
            }
            true
        }
        "Y" => {
            match val.parse() {
                Ok(offset) => cmd.offset_y = offset,
                Err(_) => set_error(cmd, "EINVAL: invalid y offset"),
            }
            true
        }
        "z" => {
            match val.parse() {
                Ok(z) => cmd.z_index = z,
                Err(_) => set_error(cmd, "EINVAL: invalid z-index"),
            }
            true
        }
        "C" => {
            match val {
                "0" => cmd.do_not_move_cursor = false,
                "1" => cmd.do_not_move_cursor = true,
                _ => set_error(cmd, "EINVAL: invalid cursor flag"),
            }
            true
        }
        _ => false,
    }
}

fn parse_payload_id_key(cmd: &mut KittyCommand, key: &str, val: &str) -> bool {
    match key {
        "S" => {
            match val.parse() {
                Ok(0) => cmd.data_size = None,
                Ok(size) => cmd.data_size = Some(size),
                Err(_) => set_error(cmd, "EINVAL: invalid data size"),
            }
            true
        }
        "O" => {
            match val.parse() {
                Ok(offset) => cmd.data_offset = offset,
                Err(_) => set_error(cmd, "EINVAL: invalid data offset"),
            }
            true
        }
        "i" => {
            cmd.id_explicit = true;
            cmd.image_id = parse_optional_non_zero_u32(cmd, val, "EINVAL: invalid image id");
            true
        }
        "I" => {
            cmd.number_explicit = true;
            cmd.image_number =
                parse_optional_non_zero_u32(cmd, val, "EINVAL: invalid image number");
            true
        }
        "p" => {
            cmd.placement_id =
                parse_optional_non_zero_u32(cmd, val, "EINVAL: invalid placement id");
            true
        }
        "P" => {
            match val.parse() {
                Ok(parent) => cmd.parent_id = Some(parent),
                Err(_) => set_error(cmd, "EINVAL: invalid parent image id"),
            }
            set_error(cmd, "ENOTSUP: relative placements are not supported");
            true
        }
        "U" => {
            match val {
                "0" => cmd.is_virtual = false,
                "1" => cmd.is_virtual = true,
                _ => set_error(cmd, "EINVAL: invalid virtual placement flag"),
            }
            true
        }
        "N" => true,
        _ => false,
    }
}

fn resolve_coordinate_delete_target(
    val: &str,
    x: u32,
    y: u32,
    z_index: i32,
) -> Option<DeleteTarget> {
    match val {
        "p" => Some(DeleteTarget::AtAbsoluteCell {
            x,
            y,
            z: None,
            free: false,
        }),
        "P" => Some(DeleteTarget::AtAbsoluteCell {
            x,
            y,
            z: None,
            free: true,
        }),
        "q" => Some(DeleteTarget::AtAbsoluteCell {
            x,
            y,
            z: Some(z_index),
            free: false,
        }),
        "Q" => Some(DeleteTarget::AtAbsoluteCell {
            x,
            y,
            z: Some(z_index),
            free: true,
        }),
        "x" => Some(DeleteTarget::ByColumn { x, free: false }),
        "X" => Some(DeleteTarget::ByColumn { x, free: true }),
        "y" => Some(DeleteTarget::ByRow { y, free: false }),
        "Y" => Some(DeleteTarget::ByRow { y, free: true }),
        "z" => Some(DeleteTarget::ByZIndex(z_index)),
        "Z" => Some(DeleteTarget::ByZIndexAndFree(z_index)),
        _ => None,
    }
}

fn resolve_delete_by_id(cmd: &mut KittyCommand, free: bool) -> DeleteTarget {
    match (cmd.image_id, cmd.placement_id) {
        (Some(id), Some(placement)) => {
            if free {
                DeleteTarget::ByIdPlacementAndFree(id, placement)
            } else {
                DeleteTarget::ByIdPlacement(id, placement)
            }
        }
        (Some(id), None) => {
            if free {
                DeleteTarget::ByIdAndFree(id)
            } else {
                DeleteTarget::ById(id)
            }
        }
        (None, _) => {
            set_error(cmd, "EINVAL: image id required");
            if free {
                DeleteTarget::AllAndFree
            } else {
                DeleteTarget::All
            }
        }
    }
}

fn resolve_delete_by_number(cmd: &mut KittyCommand, free: bool) -> DeleteTarget {
    match cmd.image_number {
        Some(number) => DeleteTarget::ByNumber {
            number,
            placement: cmd.placement_id,
            free,
        },
        None => {
            set_error(cmd, "EINVAL: image number required");
            DeleteTarget::All
        }
    }
}

fn resolve_delete_target(val: &str, cmd: &mut KittyCommand) -> DeleteTarget {
    let x = cmd.src_x.unwrap_or(0);
    let y = cmd.src_y.unwrap_or(0);

    if let Some(target) = resolve_coordinate_delete_target(val, x, y, cmd.z_index) {
        return target;
    }

    match val {
        "a" => DeleteTarget::All,
        "A" => DeleteTarget::AllAndFree,
        "c" => DeleteTarget::AtCursor,
        "C" => DeleteTarget::AtCursorAndFree,
        "i" => resolve_delete_by_id(cmd, false),
        "I" => resolve_delete_by_id(cmd, true),
        "n" | "N" => resolve_delete_by_number(cmd, val == "N"),
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
        "f" | "F" => {
            set_error(cmd, "ENOTSUP: animation is not supported");
            DeleteTarget::All
        }
        _ => {
            set_error(cmd, "ENOTSUP: unsupported delete selector");
            DeleteTarget::All
        }
    }
}

/// Parses the comma-separated control keys string into a `KittyCommand`.
#[must_use]
pub fn parse_control_keys(s: &str) -> KittyCommand {
    let mut cmd = KittyCommand::default();
    let mut delete_selector: Option<&str> = None;

    for pair in s.split(',') {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        let mut parts = pair.splitn(2, '=');
        let key = parts.next().unwrap_or("").trim();
        let val = parts.next().unwrap_or("").trim();

        let _ = parse_transmission_key(&mut cmd, key, val, &mut delete_selector)
            || parse_dimension_key(&mut cmd, key, val)
            || parse_placement_coordinate_key(&mut cmd, key, val)
            || parse_payload_id_key(&mut cmd, key, val);
    }

    if cmd.image_id.is_some() && cmd.image_number.is_some() {
        set_error(&mut cmd, "EINVAL: both image id and image number specified");
    }

    cmd.delete_target = match delete_selector {
        Some(val) => resolve_delete_target(val, &mut cmd),
        None => DeleteTarget::All,
    };

    cmd
}
