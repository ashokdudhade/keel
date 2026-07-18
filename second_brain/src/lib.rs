//! SecondBrain: deterministic, local-first code intelligence engine.

pub mod db;
pub mod error;
pub mod graph;
pub mod languages;

pub use error::{Result, SecondBrainError};
