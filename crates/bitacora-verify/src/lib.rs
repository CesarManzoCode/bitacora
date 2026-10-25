//! Verification support for bitacora: seeded generators, a simulated disk,
//! a reference model and the criteria runners of contract section 12.
//!
//! Everything here supports the engine; none of it duplicates engine logic.

pub mod criteria;
pub mod model;
pub mod rng;
pub mod simdisk;
