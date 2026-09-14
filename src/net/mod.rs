//! Frontends: the local REPL and the text / HTTP / MySQL wire servers.

// The admin surface has no public API; it is reached only through `http.rs`.
pub(crate) mod admin;
pub mod client;
pub mod http;
pub mod mysql;
pub mod protocol;
pub mod render;
pub mod server;
pub mod wire;

mod repl;

pub use repl::run_repl;
