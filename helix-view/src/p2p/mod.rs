//! Collaborative editing over iroh-gossip.
//!
//! - [`transport`] runs the actor that owns the gossip topics.
//! - [`collab`] is the editor's side: which files are shared, which buffers
//!   hold them, and how they are kept in sync.
//! - [`proto`] is what goes over the wire.

pub mod collab;
pub mod proto;
mod transport;

pub use collab::Service;
pub use transport::{Event, Request, Transport};
