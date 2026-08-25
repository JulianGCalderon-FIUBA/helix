//! Membership of a collaboration session.
//!
//! A session is a full mesh: every peer holds one connection to every other
//! peer. A peer joins by dialing any member with a ticket, and is answered
//! with the addresses of the rest of the session, which it then dials itself.
//! Opening connections is always the newcomer's job, so a peer invited by B
//! ends up connected to A and C without either of them dialing anyone.
//!
//! Two peers joining at the same time can still dial each other, since
//! neither is in the answer the other one gets. Settling that is left for
//! later.

use std::{
    collections::HashMap,
    str::FromStr,
    sync::{Arc, Mutex},
};

use anyhow::{bail, ensure, Result};
use iroh::{
    endpoint::{Connection, RecvStream, SendStream},
    protocol::{AcceptError, ProtocolHandler},
    Endpoint, EndpointAddr, EndpointId,
};
use iroh_tickets::endpoint::EndpointTicket;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use super::{
    proto::{self, Message},
    Event, ALPN,
};

/// Close code for leaving a session on purpose.
const BYE: u32 = 0;

/// A peer of the session, and the stream we talk to it over.
#[derive(Debug)]
struct Peer {
    addr: EndpointAddr,
    connection: Connection,
    /// Messages queued for the writer of this connection.
    outbox: UnboundedSender<Message>,
}

/// The peers we are currently in a session with.
#[derive(Debug, Clone)]
pub struct Session {
    endpoint: Endpoint,
    events: UnboundedSender<Event>,
    peers: Arc<Mutex<HashMap<EndpointId, Peer>>>,
}

impl Session {
    pub fn new(endpoint: Endpoint, events: UnboundedSender<Event>) -> Self {
        Self {
            endpoint,
            events,
            peers: Arc::default(),
        }
    }

    pub fn events(&self) -> &UnboundedSender<Event> {
        &self.events
    }

    fn id(&self) -> EndpointId {
        self.endpoint.id()
    }

    fn emit(&self, event: Event) {
        // The editor is gone once it stops draining events, which is not
        // something the session can do anything about.
        let _ = self.events.send(event);
    }

    fn report(&self, error: String) {
        self.emit(Event::Error(error));
    }

    /// Mints a ticket for this session.
    ///
    /// Every member can invite, so the ticket is simply our own address: the
    /// peer that redeems it reaches the whole session through us.
    pub fn ticket_new(&self) -> String {
        EndpointTicket::new(self.endpoint.addr()).to_string()
    }

    /// Joins the session advertised by a ticket.
    pub fn ticket_join(&self, ticket: &str) -> Result<()> {
        let ticket = EndpointTicket::from_str(ticket)?;
        ensure!(
            ticket.endpoint_addr().id != self.id(),
            "cannot join our own session"
        );

        self.connect(ticket.endpoint_addr().clone());

        Ok(())
    }

    /// Sends an opaque payload to every peer of the session.
    ///
    /// What the bytes mean is up to the layer above; this one only carries
    /// them, in order, to everyone currently in the session.
    pub fn broadcast(&self, data: Vec<u8>) -> Result<()> {
        let peers = self.peers.lock().unwrap();
        ensure!(!peers.is_empty(), "not in a session");

        for peer in peers.values() {
            let _ = peer.outbox.send(Message::Data(data.clone()));
        }

        Ok(())
    }

    /// Reports the peers currently in the session.
    pub fn ticket_peers(&self) -> Result<()> {
        let mut peers: Vec<_> = self.peers.lock().unwrap().keys().copied().collect();
        ensure!(!peers.is_empty(), "not in a session");
        peers.sort();

        self.emit(Event::Peers(peers));

        Ok(())
    }

    /// Leaves the session, dropping every peer.
    pub fn ticket_close(&self) -> Result<()> {
        let peers = std::mem::take(&mut *self.peers.lock().unwrap());
        ensure!(!peers.is_empty(), "not in a session");

        for (id, peer) in peers {
            peer.connection.close(BYE.into(), b"bye");
            self.emit(Event::Disconnected(id));
        }

        Ok(())
    }

    /// Dials a peer, unless it is us or one we already have.
    fn connect(&self, addr: EndpointAddr) {
        let id = addr.id;
        if id == self.id() || self.peers.lock().unwrap().contains_key(&id) {
            return;
        }

        let session = self.clone();
        tokio::spawn(async move {
            if let Err(err) = session.dial(addr).await {
                session.report(format!(
                    "failed to connect to {}: {:#}",
                    id.fmt_short(),
                    err
                ));
            }
        });
    }

    async fn dial(&self, addr: EndpointAddr) -> Result<()> {
        let connection = self.endpoint.connect(addr.clone(), ALPN).await?;
        let (mut send, mut recv) = connection.open_bi().await?;

        proto::write(
            &mut send,
            &Message::Hello {
                addr: self.endpoint.addr(),
            },
        )
        .await?;

        let Some(Message::Welcome { peers }) = proto::read(&mut recv).await? else {
            bail!("expected a welcome");
        };

        // The rest of the session is ours to dial: the peer that let us in
        // only introduces us, it does not relay for us.
        for peer in peers {
            self.connect(peer);
        }

        self.serve(addr, connection, send, recv).await;

        Ok(())
    }

    async fn answer(&self, connection: Connection) -> Result<()> {
        let (mut send, mut recv) = connection.accept_bi().await?;

        let Some(Message::Hello { addr }) = proto::read(&mut recv).await? else {
            bail!("expected a hello");
        };
        ensure!(
            addr.id == connection.remote_id(),
            "hello does not match the connection it arrived on"
        );

        proto::write(
            &mut send,
            &Message::Welcome {
                peers: self.addrs(),
            },
        )
        .await?;

        // The newcomer dials the rest of the session itself, so there is
        // nothing left for us to do but talk to it.
        self.serve(addr, connection, send, recv).await;

        Ok(())
    }

    /// Runs a connection until it ends, reading messages from the peer.
    async fn serve(
        &self,
        addr: EndpointAddr,
        connection: Connection,
        send: SendStream,
        mut recv: RecvStream,
    ) {
        let id = addr.id;
        let (outbox, queue) = unbounded_channel();

        self.enter(addr, connection.clone(), outbox);
        self.write(connection.clone(), send, queue);
        self.emit(Event::Connected(id));

        loop {
            match proto::read(&mut recv).await {
                Ok(Some(message)) => self.handle(id, message),
                // The peer finished the stream, which is how it says goodbye.
                Ok(None) => break,
                Err(err) => {
                    if connection.close_reason().is_none() {
                        self.report(format!("{}: {:#}", id.fmt_short(), err));
                    }
                    break;
                }
            }
        }

        self.leave(id, &connection);
    }

    /// Drains the outbox of a peer into its half of the session stream.
    fn write(
        &self,
        connection: Connection,
        mut send: SendStream,
        mut queue: UnboundedReceiver<Message>,
    ) {
        let session = self.clone();
        tokio::spawn(async move {
            let id = connection.remote_id();

            while let Some(message) = queue.recv().await {
                if let Err(err) = proto::write(&mut send, &message).await {
                    if connection.close_reason().is_none() {
                        session.report(format!("failed to write to {}: {:#}", id.fmt_short(), err));
                    }
                    break;
                }
            }

            let _ = send.finish();
        });
    }

    fn handle(&self, from: EndpointId, message: Message) {
        match message {
            Message::Data(data) => self.emit(Event::Message { from, data }),
            // The handshake is over; seeing it again means the peer is
            // speaking a protocol we do not.
            Message::Hello { .. } | Message::Welcome { .. } => self.report(format!(
                "unexpected handshake message from {}",
                from.fmt_short()
            )),
        }
    }

    /// Adds a peer to the session.
    fn enter(&self, addr: EndpointAddr, connection: Connection, outbox: UnboundedSender<Message>) {
        self.peers.lock().unwrap().insert(
            addr.id,
            Peer {
                addr,
                connection,
                outbox,
            },
        );
    }

    /// Drops a peer, unless it has already been removed.
    fn leave(&self, id: EndpointId, connection: &Connection) {
        if self.peers.lock().unwrap().remove(&id).is_none() {
            return;
        }

        connection.close(BYE.into(), b"bye");
        self.emit(Event::Disconnected(id));
    }

    fn addrs(&self) -> Vec<EndpointAddr> {
        self.peers
            .lock()
            .unwrap()
            .values()
            .map(|peer| peer.addr.clone())
            .collect()
    }
}

impl ProtocolHandler for Session {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let remote = connection.remote_id();

        if let Err(err) = self.answer(connection).await {
            self.report(format!(
                "failed to accept {}: {:#}",
                remote.fmt_short(),
                err
            ));
        }

        Ok(())
    }
}
