#![cfg_attr(not(doctest), doc = include_str!("../README.md"))]
#![warn(missing_docs)]
#![allow(rustdoc::redundant_explicit_links)]

pub mod blocks;
pub mod headers;
pub mod utxos;
pub mod xor;

pub use blocks::BlockParser;
pub use headers::HeaderParser;
pub use utxos::UtxoParser;
