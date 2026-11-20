//! Errors produced by the backdrop pipeline.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The destination buffer cannot hold the requested region.
    OutputTooSmall { needed: usize, got: usize },
    /// The source image does not cover the requested region.
    SourceTooSmall,
    /// Nothing to do: the region is empty after clipping.
    EmptyRegion,
    /// No usable GPU backend (no Vulkan driver, no compute queue, ...).
    GpuUnavailable(String),
    /// A GPU backend call failed after it was already initialised.
    Gpu(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutputTooSmall { needed, got } => {
                write!(f, "output buffer needs {needed} bytes, got {got}")
            }
            Self::SourceTooSmall => f.write_str("source image does not cover the region"),
            Self::EmptyRegion => f.write_str("empty backdrop region"),
            Self::GpuUnavailable(reason) => write!(f, "GPU backend unavailable: {reason}"),
            Self::Gpu(reason) => write!(f, "GPU backend error: {reason}"),
        }
    }
}

impl std::error::Error for Error {}
