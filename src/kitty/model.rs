//! Protocol data structures, image models, and event types for Kitty graphics protocol.

/// Kitty graphics action requested by the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KittyAction {
    #[default]
    TransmitAndDisplay,
    TransmitAndDisplayWithResponse,
    Query,
    Place,
    Delete,
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
    ById(u32),
    ByPlacement(u32),
    AtCursor,
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
    pub placement_id: Option<u32>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub cols: Option<u32>,
    pub rows: Option<u32>,
    pub offset_x: u32,
    pub offset_y: u32,
    pub z_index: i32,
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
    pub offset_x: u32,
    pub offset_y: u32,
    pub z_index: i32,
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
