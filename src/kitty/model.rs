//! Domain types for one Kitty graphics command and its placement on the grid.

/// Kitty graphics action requested by the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KittyAction {
    #[default]
    Transmit,
    TransmitAndDisplay,
    TransmitAndDisplayWithResponse,
    Query,
    Place,
    Delete,
    /// Animation frame transmission (`a=f`); recognized but unsupported.
    TransmitFrame,
    /// Animation control (`a=a`); recognized but unsupported.
    ControlAnimation,
    /// Animation frame composition (`a=c`); recognized but unsupported.
    ComposeFrame,
}

/// Payload compression requested through the `o=` key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KittyCompression {
    /// RFC 1950 zlib stream.
    Zlib,
}

/// Pixel format of image payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KittyFormat {
    #[default]
    Rgba32,
    Rgb24,
    Png,
}

/// Transmission medium for image payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KittyMedium {
    #[default]
    Direct,
    File,
    TempFile,
    SharedMemory,
}

/// Delete target selector for `a=d`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeleteTarget {
    #[default]
    All,
    AllAndFree,
    ById(u32),
    ByIdAndFree(u32),
    /// Delete one placement of an image (`d=i` combined with `p=`).
    ByIdPlacement(u32, u32),
    ByIdPlacementAndFree(u32, u32),
    /// Newest image with a client image number (`d=n`), resolved by the parser.
    ByNumber {
        number: u32,
        placement: Option<u32>,
        free: bool,
    },
    AtCursor,
    AtCursorAndFree,
    /// Delete placements intersecting an absolute cell (`d=p`/`d=q`), 1-based.
    AtAbsoluteCell {
        x: u32,
        y: u32,
        z: Option<i32>,
        free: bool,
    },
    /// Delete placements intersecting a column (`d=x`), 1-based.
    ByColumn {
        x: u32,
        free: bool,
    },
    /// Delete placements intersecting a row (`d=y`), 1-based.
    ByRow {
        y: u32,
        free: bool,
    },
    /// Delete placements by z-index (`d=z`).
    ByZIndex(i32),
    ByZIndexAndFree(i32),
    /// Delete images whose id is within an inclusive range (`d=r`).
    ByIdRange {
        lo: u32,
        hi: u32,
        free: bool,
    },
}

/// Parsed Kitty APC command parameters.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct KittyCommand {
    pub action: KittyAction,
    pub format: KittyFormat,
    pub medium: KittyMedium,
    pub delete_target: DeleteTarget,
    pub image_id: Option<u32>,
    /// Whether the client sent an explicit `i=` key. The protocol only requires an
    /// `OK`/error acknowledgement when the client opted in by supplying an image id.
    pub id_explicit: bool,
    /// Image number requested through `I=`; the terminal allocates the actual id.
    pub image_number: Option<u32>,
    pub number_explicit: bool,
    pub placement_id: Option<u32>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub cols: Option<u32>,
    pub rows: Option<u32>,
    /// Source crop rectangle (x, y, w, h) within the image.
    pub src_x: Option<u32>,
    pub src_y: Option<u32>,
    pub src_w: Option<u32>,
    pub src_h: Option<u32>,
    /// Cell-relative pixel offset (X, Y).
    pub offset_x: u32,
    pub offset_y: u32,
    pub z_index: i32,
    /// Number of bytes to read for file/shared-memory payloads (`S=`).
    pub data_size: Option<u64>,
    /// Byte offset within a file/shared-memory payload (`O=`).
    pub data_offset: u64,
    /// Optional payload compression (`o=`).
    pub compression: Option<KittyCompression>,
    /// Whether `f=`/`t=` were explicitly supplied, for capability probes.
    pub format_explicit: bool,
    pub medium_explicit: bool,
    /// Standard error discovered while parsing the control data.
    pub error: Option<&'static str>,
    /// Parent image id for relative placements (`P=`), which are unsupported.
    pub parent_id: Option<u32>,
    pub more_chunks: bool,
    pub do_not_move_cursor: bool,
    pub quiet: u8,
    pub is_virtual: bool,
}

/// Loaded RGBA image data ready for GPU texture upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageData {
    pub id: u32,
    pub width: u32,
    pub height: u32,
    pub rgba: Option<Vec<u8>>,
}

impl ImageData {
    #[must_use]
    pub fn new(id: u32, width: u32, height: u32, rgba: Vec<u8>) -> Self {
        Self {
            id,
            width,
            height,
            rgba: Some(rgba),
        }
    }

    #[must_use]
    pub fn byte_size(&self) -> usize {
        (self.width as usize)
            .saturating_mul(self.height as usize)
            .saturating_mul(4)
    }
}

/// On-screen image placement anchored to grid coordinates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImagePlacement {
    pub image_id: u32,
    pub placement_id: u32,
    pub line: usize,
    pub col: usize,
    pub cols: usize,
    pub rows: usize,
    pub src_x: u32,
    pub src_y: u32,
    pub src_w: Option<u32>,
    pub src_h: Option<u32>,
    pub offset_x: u32,
    pub offset_y: u32,
    pub z_index: i32,
}

impl Default for ImagePlacement {
    fn default() -> Self {
        Self {
            image_id: 0,
            placement_id: 0,
            line: 0,
            col: 0,
            cols: 1,
            rows: 1,
            src_x: 0,
            src_y: 0,
            src_w: None,
            src_h: None,
            offset_x: 0,
            offset_y: 0,
            z_index: 0,
        }
    }
}

/// Event emitted by the Kitty APC parser.
#[derive(Debug, Clone, PartialEq)]
pub enum KittyEvent {
    /// Transmit and place new image
    Transmit {
        command: KittyCommand,
        image: ImageData,
    },
    /// Place an existing image by ID
    Place { command: KittyCommand },
    /// Delete image(s)
    Delete { target: DeleteTarget },
    /// Query support or acknowledge command; contains bytes to write to PTY
    Response(Vec<u8>),
}
