use std::fs;
use std::io;

use base64::Engine;
use base64::prelude::BASE64_STANDARD;
use nix::fcntl::OFlag;
use nix::sys::mman::shm_open;
use nix::sys::stat::Mode;

use crate::kitty::command::parse_control_keys;
use crate::kitty::model::{DeleteTarget, KittyAction, KittyCommand, KittyEvent, KittyMedium};
use crate::kitty::payload::{MAX_SHM_PAYLOAD, decode_image_data, read_shm_bytes, read_shm_payload};
use crate::kitty::{KittyParser, MAX_RECYCLED_CLEAN_BYTES, kitty_response};

#[test]
fn shared_memory_loads_images_and_rejects_oversized_payloads() {
    use nix::sys::mman::shm_unlink;
    use std::io::Write;

    struct ShmName(String);
    impl Drop for ShmName {
        fn drop(&mut self) {
            let _ = shm_unlink(self.0.as_str());
        }
    }

    let name = format!("/ftty-kitty-test-{}", std::process::id());
    let fd = shm_open(
        name.as_str(),
        OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_RDWR,
        Mode::S_IRUSR | Mode::S_IWUSR,
    )
    .unwrap();
    let name = ShmName(name);
    let mut file = fs::File::from(fd);
    assert_eq!(read_shm_payload(&name.0).unwrap(), Vec::<u8>::new());

    let pixels = [255, 128, 0, 255];
    file.write_all(&pixels).unwrap();
    let command = KittyCommand {
        medium: KittyMedium::SharedMemory,
        width: Some(1),
        height: Some(1),
        ..Default::default()
    };
    let image = decode_image_data(7, &command, BASE64_STANDARD.encode(&name.0).as_bytes()).unwrap();
    assert_eq!(image.rgba.as_deref(), Some(pixels.as_slice()));
    assert_eq!((image.id, image.width, image.height), (7, 1, 1));

    // A sparse object can exceed the cap without consuming that much RAM.
    file.set_len(MAX_SHM_PAYLOAD + 1).unwrap();
    assert_eq!(
        read_shm_payload(&name.0).unwrap_err().kind(),
        io::ErrorKind::InvalidData,
    );
}

#[test]
fn shared_memory_size_changes_are_rejected() {
    use nix::sys::memfd::{MFdFlags, memfd_create};

    for (initial, changed, error_kind) in [
        (4, 2, io::ErrorKind::UnexpectedEof),
        (4, 8, io::ErrorKind::InvalidData),
        (0, 1, io::ErrorKind::InvalidData),
    ] {
        let fd = memfd_create(c"ftty-shm-resize-test", MFdFlags::MFD_CLOEXEC).unwrap();
        let file = fs::File::from(fd);
        file.set_len(initial).unwrap();
        let observed_size = file.metadata().unwrap().len();

        // Deterministically simulate the sender resizing after the size query.
        file.set_len(changed).unwrap();
        assert_eq!(
            read_shm_bytes(file, observed_size).unwrap_err().kind(),
            error_kind,
        );
    }
}

#[test]
fn test_parse_kitty_query() {
    let mut parser = KittyParser::new();
    let (text, events) = parser.filter_bytes(b"\x1b_Gi=42,a=q;\x1b\\");
    assert!(text.is_empty());
    assert_eq!(events.len(), 1);
    match &events[0] {
        KittyEvent::Response(resp) => {
            assert_eq!(resp, b"\x1b_Gi=42;OK\x1b\\");
        }
        other => panic!("Expected query response, got {:?}", other),
    }
}

#[test]
fn test_parse_kitty_direct_rgba() {
    let mut parser = KittyParser::new();
    // 2x1 RGBA: [255, 0, 0, 255, 0, 255, 0, 255]
    let raw_pixels = [255u8, 0, 0, 255, 0, 255, 0, 255];
    let b64 = BASE64_STANDARD.encode(raw_pixels);
    let seq = format!("\x1b_Ga=t,f=32,s=2,v=1,i=10;{b64}\x1b\\");

    let (text, events) = parser.filter_bytes(seq.as_bytes());
    assert!(text.is_empty());
    assert_eq!(events.len(), 1);

    match &events[0] {
        KittyEvent::Transmit { command, image } => {
            assert_eq!(command.image_id, Some(10));
            assert_eq!(image.width, 2);
            assert_eq!(image.height, 1);
            assert_eq!(image.rgba.as_deref(), Some(raw_pixels.as_slice()));
        }
        other => panic!("Expected Transmit, got {:?}", other),
    }
}

#[test]
fn test_parse_kitty_chunking() {
    let mut parser = KittyParser::new();
    let raw_pixels = [10u8, 20, 30, 255];
    let b64 = BASE64_STANDARD.encode(raw_pixels);
    let part1 = &b64[..2];
    let part2 = &b64[2..];

    let seq1 = format!("\x1b_Ga=t,f=32,s=1,v=1,i=5,m=1;{part1}\x1b\\");
    let seq2 = format!("\x1b_Gm=0;{part2}\x1b\\");

    let (t1, e1) = parser.filter_bytes(seq1.as_bytes());
    assert!(t1.is_empty());
    assert!(e1.is_empty());

    let (t2, e2) = parser.filter_bytes(seq2.as_bytes());
    assert!(t2.is_empty());
    assert_eq!(e2.len(), 1);

    match &e2[0] {
        KittyEvent::Transmit { command, image } => {
            assert_eq!(command.image_id, Some(5));
            assert_eq!(image.rgba.as_deref(), Some(raw_pixels.as_slice()));
        }
        other => panic!("Expected Transmit, got {:?}", other),
    }
}

#[test]
fn test_parse_kitty_delete() {
    let mut parser = KittyParser::new();
    let (text, events) = parser.filter_bytes(b"\x1b_Ga=d,d=a;\x1b\\");
    assert!(text.is_empty());
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0],
        KittyEvent::Delete {
            target: DeleteTarget::All
        }
    );
}

#[test]
fn test_parse_kitty_png_format() {
    // Create 1x1 red PNG
    let mut png_bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png_bytes, 1, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&[255, 0, 0, 255]).unwrap();
    }

    let b64 = BASE64_STANDARD.encode(&png_bytes);
    let seq = format!("\x1b_Ga=t,f=100,i=99;{b64}\x1b\\");

    let mut parser = KittyParser::new();
    let (text, events) = parser.filter_bytes(seq.as_bytes());
    assert!(text.is_empty());
    assert_eq!(events.len(), 1);

    match &events[0] {
        KittyEvent::Transmit { command, image } => {
            assert_eq!(command.image_id, Some(99));
            assert_eq!(image.width, 1);
            assert_eq!(image.height, 1);
            assert_eq!(image.rgba.as_deref(), Some([255, 0, 0, 255].as_slice()));
        }
        other => panic!("Expected Transmit with PNG, got {:?}", other),
    }
}

#[test]
fn test_kitty_transmit_with_response() {
    let raw = [0u8, 0, 255, 255];
    let b64 = BASE64_STANDARD.encode(raw);
    let seq = format!("\x1b_Ga=T,f=32,s=1,v=1,i=77;{b64}\x1b\\");

    let mut parser = KittyParser::new();
    let (text, events) = parser.filter_bytes(seq.as_bytes());
    assert!(text.is_empty());
    assert_eq!(events.len(), 1);

    match &events[0] {
        KittyEvent::Transmit { command, image } => {
            assert_eq!(command.action, KittyAction::TransmitAndDisplayWithResponse);
            assert_eq!(command.image_id, Some(77));
            assert_eq!(image.rgba.as_deref(), Some(raw.as_slice()));
        }
        other => panic!("Expected Transmit with response, got {:?}", other),
    }
}

#[test]
fn test_split_apc_framing() {
    let mut parser = KittyParser::new();
    let (t1, e1) = parser.filter_bytes(b"hello\x1b");
    assert_eq!(t1, b"hello");
    assert!(e1.is_empty());

    let (t2, e2) = parser.filter_bytes(b"_Gi=1,a=q;\x1b");
    assert!(t2.is_empty());
    assert!(e2.is_empty());

    let (t3, e3) = parser.filter_bytes(b"\\world");
    assert_eq!(t3, b"world");
    assert_eq!(e3.len(), 1);
    assert_eq!(e3[0], KittyEvent::Response(b"\x1b_Gi=1;OK\x1b\\".to_vec()));
}

#[test]
fn test_order_independent_delete() {
    let cmd = parse_control_keys("d=i,i=7,a=d");
    assert_eq!(cmd.action, KittyAction::Delete);
    assert_eq!(cmd.delete_target, DeleteTarget::ById(7));
}

#[test]
fn explicit_image_id_tracking() {
    assert!(parse_control_keys("a=t,i=7").id_explicit);
    assert!(parse_control_keys("i=7").image_id == Some(7));
    assert!(!parse_control_keys("a=t").id_explicit);
    assert!(parse_control_keys("a=t").image_id.is_none());
}

#[test]
fn response_encoding_includes_only_real_placement_ids() {
    assert_eq!(
        kitty_response(3, None, "OK"),
        b"\x1b_Gi=3;OK\x1b\\".to_vec()
    );
    assert_eq!(
        kitty_response(3, Some(0), "OK"),
        b"\x1b_Gi=3;OK\x1b\\".to_vec()
    );
    assert_eq!(
        kitty_response(3, Some(5), "ENOENT:image not found"),
        b"\x1b_Gi=3,p=5;ENOENT:image not found\x1b\\".to_vec()
    );
}

#[test]
fn test_kitty_probe_interleaving() {
    let mut kitty_parser = KittyParser::new();
    let query = b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b[c\x1b[16t\x1b]11;?\x07\x1b[5n";
    let (clean, events) = kitty_parser.filter_bytes(query);
    println!("clean: {:?}", std::str::from_utf8(&clean).unwrap());
    println!("events: {:?}", events);
}

#[test]
fn chunked_transmit_preserves_explicit_id_and_quiet() {
    let b64 = BASE64_STANDARD.encode([1u8, 2, 3, 4]);
    let first = format!("\x1b_Ga=t,f=32,s=1,v=1,i=9,q=0,m=1;{}\x1b\\", &b64[..2]);
    let last = format!("\x1b_Gm=0;{}\x1b\\", &b64[2..]);
    let mut parser = KittyParser::new();
    assert!(parser.filter_bytes(first.as_bytes()).1.is_empty());
    let (_, events) = parser.filter_bytes(last.as_bytes());
    match &events[0] {
        KittyEvent::Transmit { command, .. } => {
            assert!(command.id_explicit);
            assert_eq!(command.image_id, Some(9));
            assert_eq!(command.quiet, 0);
        }
        other => panic!("Expected Transmit, got {other:?}"),
    }
}

#[test]
fn test_filter_fast_path_direct_borrow() {
    let mut parser = KittyParser::new();

    // 1. Plain text is fast-path eligible
    let plain = b"Hello world, normal text from cat or grep\n";
    assert!(parser.is_fast_path(plain));
    let (clean, events) = parser.filter_bytes(plain);
    assert_eq!(clean, plain);
    assert!(events.is_empty());

    // 2. ANSI color and cursor escapes are also fast-path eligible
    let ansi = b"\x1b[31mRed text\x1b[0m and \x1b[10;20Hcursor\x1b]8;;https://x.com\x1b\\";
    assert!(parser.is_fast_path(ansi));
    let (clean, events) = parser.filter_bytes(ansi);
    assert_eq!(clean, ansi);
    assert!(events.is_empty());

    // 3. Kitty graphics sequence requires slow-path filtering
    let kitty_payload = b"before\x1b_Gi=42,a=q;\x1b\\after";
    assert!(!parser.is_fast_path(kitty_payload));
    let (text, events) = parser.filter_bytes(kitty_payload);
    assert_eq!(text, b"beforeafter");
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0],
        KittyEvent::Response(b"\x1b_Gi=42;OK\x1b\\".to_vec())
    );
    // Events must be transferred out; parser must not retain image/event data
    assert!(parser.events_scratch.is_empty());
}

#[test]
fn test_scratch_buffer_reused_without_reallocation() {
    let mut parser = KittyParser::new();
    let payload = b"start\x1b_Gi=10,a=q;\x1b\\end";

    // First call populates scratch buffer
    let _ = parser.filter_bytes(payload);
    let cap_text = parser.clean_scratch.capacity();
    assert!(cap_text > 0);
    // Events are drained so events_scratch does not keep image memory
    assert!(parser.events_scratch.is_empty());

    // Subsequent calls reuse existing capacity
    for _ in 0..100 {
        let _ = parser.filter_bytes(payload);
        assert_eq!(parser.clean_scratch.capacity(), cap_text);
        assert!(parser.events_scratch.is_empty());
    }
}

#[test]
fn test_slow_filter_recycles_clean_buffer_by_move() {
    let mut parser = KittyParser::new();
    let payload = b"start\x1b_Gi=10,a=q;\x1b\\end";

    // The hot path hands the cleaned bytes over by move, leaving the parser scratch empty.
    let (text, events) = parser.filter_bytes_slow(payload);
    assert_eq!(text.as_slice(), b"startend");
    assert!(parser.clean_scratch.is_empty());
    let cap_text = text.capacity();
    assert!(cap_text > 0);
    // Events are transferred out, so the parser retains no event storage.
    assert!(parser.events_scratch.is_empty());
    drop(events);

    // Recycling retains the capacity so the next slow filter reuses the same allocation.
    parser.recycle_clean_buffer(text);
    assert_eq!(parser.clean_scratch.capacity(), cap_text);

    let (text2, _) = parser.filter_bytes_slow(payload);
    assert_eq!(text2.as_slice(), b"startend");
    assert!(text2.capacity() >= cap_text);
    parser.recycle_clean_buffer(text2);
}

#[test]
fn test_recycle_clean_buffer_drops_oversized_allocations() {
    let mut parser = KittyParser::new();
    parser.recycle_clean_buffer(Vec::with_capacity(MAX_RECYCLED_CLEAN_BYTES + 1));
    assert_eq!(parser.clean_scratch.capacity(), 0);

    parser.recycle_clean_buffer(Vec::with_capacity(4096));
    assert!(parser.clean_scratch.capacity() >= 4096);
}

#[test]
fn test_split_escape_crosses_chunk_into_processed() {
    let mut parser = KittyParser::new();
    // Chunk 1 ends with escape (not fast path)
    let chunk1 = b"text\x1b";
    assert!(!parser.is_fast_path(chunk1));
    let (text, events) = parser.filter_bytes(chunk1);
    assert_eq!(text, b"text");
    assert!(events.is_empty());

    // Chunk 2 completes the Kitty sequence
    let chunk2 = b"_Gi=99,a=q;\x1b\\more";
    assert!(!parser.is_fast_path(chunk2));
    let (text, events) = parser.filter_bytes(chunk2);
    assert_eq!(text, b"more");
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0],
        KittyEvent::Response(b"\x1b_Gi=99;OK\x1b\\".to_vec())
    );
    assert!(parser.events_scratch.is_empty());
}

#[test]
fn test_is_fast_path() {
    let mut parser = KittyParser::new();
    // Plain text is fast path
    assert!(parser.is_fast_path(b"Hello world\n"));
    // ANSI escape codes (colors, cursor, hyperlinks) are fast path
    assert!(parser.is_fast_path(b"\x1b[31mRed\x1b[0m \x1b[10;20H \x1b]8;;https://x.com\x1b\\"));
    // Kitty sequence is NOT fast path
    assert!(!parser.is_fast_path(b"\x1b_Gi=1,a=q;\x1b\\"));
    // Chunk ending in 0x1b is NOT fast path (could split across read boundaries)
    assert!(!parser.is_fast_path(b"hello\x1b"));

    // While in APC, even plain text is NOT fast path
    parser.in_apc = true;
    assert!(!parser.is_fast_path(b"more data"));
}

#[test]
fn test_image_remains_valid_for_placements_after_cpu_buffer_unload() {
    use crate::grid::Grid;
    use crate::kitty::model::{ImageData, ImagePlacement};

    let mut grid = Grid::new(80, 24, 100);
    let image = ImageData::new(42, 64, 64, vec![255; 64 * 64 * 4]);
    assert_eq!(image.byte_size(), 64 * 64 * 4);
    grid.add_image(image);

    // Simulate Renderer unloading CPU pixel buffer after OpenGL upload
    if let Some(img) = grid.images.get_mut(&42) {
        assert!(img.rgba.is_some());
        img.rgba = None;
    }

    // Grid tracking, size calculation and placement remain fully intact
    let img = grid.images.get(&42).unwrap();
    assert!(img.rgba.is_none());
    assert_eq!(img.byte_size(), 64 * 64 * 4);

    grid.add_placement(ImagePlacement {
        image_id: 42,
        placement_id: 1,
        line: 0,
        col: 0,
        cols: 10,
        rows: 5,
        offset_x: 0,
        offset_y: 0,
        z_index: 0,
    });
    assert_eq!(grid.placements.len(), 1);
    assert_eq!(grid.placements[0].image_id, 42);

    // Eviction budget accurately counts unloaded images
    let total_stored: usize = grid.images.values().map(ImageData::byte_size).sum();
    assert_eq!(total_stored, 64 * 64 * 4);
}

#[test]
fn test_convenience_filter_drops_oversized_scratch_allocation() {
    let mut parser = KittyParser::new();
    // Large clean output followed by a lone ESC forces the slow path with a big scratch buffer.
    let mut payload = vec![b'x'; MAX_RECYCLED_CLEAN_BYTES + 16];
    payload.push(0x1b);

    let (text, _) = parser.filter_bytes(&payload);
    assert_eq!(text.len(), MAX_RECYCLED_CLEAN_BYTES + 16);
    // The retained scratch must respect the same bound as the recycled hot path.
    assert!(parser.clean_scratch.capacity() <= MAX_RECYCLED_CLEAN_BYTES);

    // Small inputs keep reusing a bounded scratch capacity.
    let (_, _) = parser.filter_bytes(b"hello\x1b");
    let cap = parser.clean_scratch.capacity();
    assert!(cap > 0 && cap <= MAX_RECYCLED_CLEAN_BYTES);
}
