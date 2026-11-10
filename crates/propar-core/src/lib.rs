#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]
//! ProPar frame and message codecs, independent of catalogs and transports.
//!
//! The default build uses only `core`, needs no allocator, and has no dependencies.
//! Enable `std` for owned frame and message convenience types.

#[cfg(all(test, not(feature = "std")))]
extern crate std;

pub mod frame;
pub mod message;

/// FLOW-BUS address of the instrument directly connected to the port.
pub const LOCAL_NODE: u8 = 0x80;
/// Factory default serial baud rate (8 data bits, no parity, one stop bit).
pub const DEFAULT_BAUD_RATE: u32 = 38_400;
