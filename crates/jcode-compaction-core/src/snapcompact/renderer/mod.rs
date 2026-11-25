//! Renderer submodule: text→PNG rasterizer.

pub mod snapcompact_render;

pub use snapcompact_render::{
    SnapcompactError, SnapcompactRenderOptions, render_snapcompact_png, snapcompact_supported_chars,
};
