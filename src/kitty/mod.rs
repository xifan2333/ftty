//! Native Kitty Graphics Protocol APC sequence parsing, chunking, and image decoding.

pub mod command;
pub mod model;
pub mod payload;
pub mod store;
mod stream;

pub use store::{
    GraphicContext, GraphicOutcome, GraphicStore, MAX_PLACEMENTS, MAX_STORED_IMAGE_BYTES,
    MAX_STORED_IMAGES, collect_placeholder_ids,
};
pub use stream::{
    KittyParser, KittyStreamChunk, MAX_RECYCLED_CLEAN_BYTES, kitty_response, kitty_response_full,
};
