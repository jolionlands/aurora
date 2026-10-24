//! The pure Rust Linux image decoder has no COM apartment requirement.
use anyhow::Result;

pub struct ComApartment;

impl ComApartment {
    pub fn initialize() -> Result<Self> {
        Ok(Self)
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {}
}
