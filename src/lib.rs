#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]
// The README doctests exercise both serial backends.
#![cfg_attr(
    any(not(doctest), all(feature = "serial", feature = "async-serial")),
    doc = include_str!("../README.md")
)]

#[cfg(all(test, not(feature = "std")))]
extern crate std;

pub use propar_catalog as catalog;
pub use propar_core::{DEFAULT_BAUD_RATE, LOCAL_NODE, frame, message};
#[cfg(feature = "embedded-async")]
pub use propar_embedded as embedded;

#[cfg(any(feature = "blocking", feature = "async"))]
mod client;
#[cfg(any(feature = "blocking", feature = "async"))]
mod discovery;
#[cfg(feature = "async")]
pub use client::asynchronous;
#[cfg(feature = "blocking")]
pub use client::blocking;
#[cfg(any(feature = "blocking", feature = "async"))]
pub use client::{Broadcast, Error, Node};
#[cfg(feature = "async")]
mod async_io;
#[cfg(feature = "blocking")]
mod blocking_io;
