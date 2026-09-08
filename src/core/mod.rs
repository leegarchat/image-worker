//! Shared core: error type, image container (raw/sparse), utils.
//! Single core used by both `read` (inspect) and `write` (repack).

pub mod buf;
pub mod error;
pub mod image;
pub mod util;

pub use error::Error;
pub use image::Image;
