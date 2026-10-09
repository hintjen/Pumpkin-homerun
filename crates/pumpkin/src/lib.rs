//! Library target for embedding Pumpkin. The server itself lives in
//! `pumpkin-core`; this crate only adds the iOS FFI entry points, which need
//! both the core and the wasm plugin host.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

#[cfg(target_os = "ios")]
pub mod ios;
// Compiled under `cfg(test)` on every host too, so the one piece of the iOS
// bridge with real logic stays covered by ordinary CI. See the module docs.
#[cfg(any(target_os = "ios", test))]
mod log_ring;
