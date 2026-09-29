//! Image-to-ASCII/ANSI conversion and rendering, as used by the `halftone`
//! CLI, plus (with the `generate` feature) image generation through OpenAI.
//! Build with `default-features = false` to use just the conversion library
//! without the CLI's argument parsing or any HTTP dependencies.

#[cfg(feature = "generate")]
pub mod openai;
pub mod pipeline;
