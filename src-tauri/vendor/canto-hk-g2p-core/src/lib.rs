//! Pure-Rust portion of canto-hk-g2p 2.6.1.
//!
//! Pear Music Widget does not embed Python. This vendored crate intentionally
//! omits the upstream PyO3 wrapper and exposes the upstream Rust pipeline only.

mod aa_dei_sandhi;
mod address_sandhi;
mod classifier_reduplication;
pub mod dict;
mod g2p;
mod normalizer;
mod pipeline;
mod romanized_slang;
mod segment;
mod separable;
mod tone_util;
mod user_dict;

pub use pipeline::Pipeline;
