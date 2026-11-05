//! The editor is reponsible for managing the p2p module: it connects the
//! transport to the session, and maps data to it's internal state.

pub mod crdt;
pub mod net;
pub mod session;
pub mod wire;
