//! Peer-to-peer collaboration, split in layers that each know only what they
//! need:
//!
//! - [`net`] is the transport. It owns our identity, connections and gossip
//!   swarms, and moves opaque bytes. It knows nothing about sessions or
//!   documents.
//! - [`session`] is the protocol, as a sans-IO state machine. It knows the
//!   session's topic, the shared documents and their owners. It takes received
//!   messages and local edits, and returns what to send and what changed.
//! - [`crdt`] merges a document's text, and [`wire`] encodes messages.
//!
//! The editor drives both: it feeds the session, hands its output to the
//! transport, and maps shared documents to buffers. It also owns the
//! lifetimes. The transport lives as long as the editor, and only when p2p is
//! enabled. A `Session` lives for one collaborative session.

pub mod crdt;
pub mod net;
pub mod session;
pub mod wire;
