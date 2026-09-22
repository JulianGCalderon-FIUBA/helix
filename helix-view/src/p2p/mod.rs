//! Collaborative editing between peers, without a server.
//!
//! Split in layers, each only knowing about the ones below it:
//!
//! - [`session`]: what sharing a document means. Opens buffers other peers
//!   share, applies their edits and broadcasts ours. The only layer that
//!   touches the editor.
//! - [`wire`]: the messages peers exchange, and their encoding.
//! - [`net`]: moves bytes between the members of a session, over iroh-gossip.
//! - [`crdt`]: translates Helix's edits to and from cola, so concurrent edits
//!   converge to the same text on every peer.
//!
//! [`net`] and [`crdt`] know nothing about each other, or about documents.

pub mod crdt;
pub mod net;
pub mod session;
pub mod wire;
