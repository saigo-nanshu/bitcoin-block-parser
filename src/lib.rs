#![doc = include_str!("../README.md")]
#![warn(missing_docs)]
#![allow(rustdoc::redundant_explicit_links)]

pub mod blocks;
pub mod headers;
pub mod utxos;

pub use blocks::BlockParser;
pub use blocks::DefaultParser;
pub use headers::HeaderParser;
