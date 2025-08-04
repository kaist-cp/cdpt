#![feature(likely_unlikely)]
#![feature(strict_provenance_atomic_ptr)]
#[macro_use]
extern crate cfg_if;

mod collector;
mod epoch;
mod guards;
mod internal;
mod pointers;
mod sync;
mod task;
mod tls;

pub use guards::{Guard, Handle};
pub use pointers::{AtomicShared, Local, Shared, TraceObj, TracePtr};
pub use tls::*;
