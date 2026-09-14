//! Read-only introspection for the web console (`docs/web_frontend.md` §3.1).
//!
//! The console has to show what the engine keeps to itself: the pages, the
//! buffer pool, the operator tree. Those live behind private and `pub(crate)`
//! fields, and widening each of them to `pub` would tear a permanent opening in
//! the encapsulation.
//!
//! So the code that looks at them lives *here*, inside the crate, where those
//! fields are already visible, and what leaves is flat, read-only data. Nothing
//! in this module mutates, opens a transaction or takes a write lock: it reads
//! the catalog and the plan, and hands back something a JSON writer can spell.

pub mod plan;
