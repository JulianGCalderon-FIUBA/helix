//! Peer to peer transport for collaboration sessions.
//!
//! [`Service`] owns an iroh endpoint on its own task and talks to the editor
//! over two channels: [`Payload`]s go in, [`Event`]s come out. The editor
//! never touches the network, and this module never looks at documents.

mod proto;
mod session;

use std::time::Duration;

use iroh::{endpoint::presets, protocol::Router, Endpoint, EndpointId};
use tokio::sync::mpsc::{unbounded_channel, Sender, UnboundedSender};
use tokio_stream::wrappers::UnboundedReceiverStream;

use session::Session;

pub const ALPN: &[u8] = b"helix/session/0";

/// Something that happened in the session, for the editor to render.
#[derive(Debug)]
pub enum Event {
    Connected(EndpointId),
    Ping(EndpointId),
    Pong(EndpointId, Duration),
    Disconnected(EndpointId),
    Peers(Vec<EndpointId>),
    /// A payload from a peer, in the bytes the sender handed to the session.
    Message {
        from: EndpointId,
        data: Vec<u8>,
    },
    Error(String),
}

/// Something for the session to do, asked for by the editor.
#[derive(Debug)]
pub enum Payload {
    TicketNew(Sender<String>),
    TicketJoin(String),
    TicketPing,
    TicketPeers,
    TicketClose,
    /// Sends an opaque payload to every peer.
    Broadcast(Vec<u8>),
    /// Sends an opaque payload to a single peer.
    Send(EndpointId, Vec<u8>),
}

pub struct Service {
    pub incoming: UnboundedReceiverStream<Event>,
    pub server_tx: UnboundedSender<Payload>,
}

impl Service {
    pub fn new() -> Self {
        let (events_tx, events_rx) = unbounded_channel();
        let (payloads_tx, mut payloads_rx) = unbounded_channel();

        tokio::spawn(async move {
            let endpoint = Endpoint::builder(presets::N0).bind().await.unwrap();
            endpoint.online().await;

            let session = Session::new(endpoint.clone(), events_tx);

            let _router = Router::builder(endpoint)
                .accept(ALPN, session.clone())
                .spawn();

            while let Some(payload) = payloads_rx.recv().await {
                let result = match payload {
                    Payload::TicketNew(chan) => {
                        let _ = chan.send(session.ticket_new()).await;
                        Ok(())
                    }
                    Payload::TicketJoin(ticket) => session.ticket_join(&ticket),
                    Payload::TicketPing => session.ticket_ping(),
                    Payload::TicketPeers => session.ticket_peers(),
                    Payload::TicketClose => session.ticket_close(),
                    Payload::Broadcast(data) => session.broadcast(data),
                    Payload::Send(to, data) => session.send(to, data),
                };

                if let Err(err) = result {
                    let _ = session.events().send(Event::Error(format!("{:#}", err)));
                }
            }
        });

        Service {
            incoming: UnboundedReceiverStream::new(events_rx),
            server_tx: payloads_tx,
        }
    }
}

impl Service {
    /// Sends an opaque payload to every peer of the session.
    ///
    /// This is the seam the collaboration layer sits on: it decides what the
    /// bytes mean, and receives the ones peers send back as
    /// [`Event::Message`].
    pub fn broadcast(&self, data: Vec<u8>) {
        let _ = self.server_tx.send(Payload::Broadcast(data));
    }

    /// Sends an opaque payload to a single peer of the session.
    pub fn send(&self, to: EndpointId, data: Vec<u8>) {
        let _ = self.server_tx.send(Payload::Send(to, data));
    }
}

impl Default for Service {
    fn default() -> Self {
        Self::new()
    }
}
