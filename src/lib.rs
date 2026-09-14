#![doc = include_str!("../README.md")]

pub mod client;
pub mod error;

pub use client::{SgcClient, SgcEvent};
pub use error::SgcError;
pub use simple_graphics_protocol::{InputResource, Resource};
