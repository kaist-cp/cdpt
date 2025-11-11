#![feature(cold_path)]
#![feature(likely_unlikely)]
#![feature(vec_push_within_capacity)]

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
