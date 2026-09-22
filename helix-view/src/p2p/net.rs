//! The transport: our iroh endpoint and the gossip topic of the session.
//!
//! It moves opaque bytes between the members of a session and knows nothing
//! about documents or messages, which is [`session`](super::session)'s and
//! [`wire`](super::wire)'s job.
//!
//! All the networking runs on one spawned task, the [`Node`], and the editor
//! talks to it through channels. The editor is synchronous and iroh is async,
//! and a single task owning the topic means none of its state needs locks.

use std::future::Future;

use anyhow::{ensure, Result};
use bytes::Bytes;
pub use iroh::EndpointId;
use iroh::{
    address_lookup::memory::MemoryLookup, endpoint::presets, protocol::Router, Endpoint,
    EndpointAddr, SecretKey,
};
use iroh_gossip::{
    api::{ApiError, Event as GossipEvent, GossipTopic},
    Gossip, TopicId, ALPN,
};
use iroh_tickets::{ParseError, Ticket};
use n0_future::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::{
    mpsc::{unbounded_channel, UnboundedSender},
    oneshot,
};
use tokio_stream::wrappers::UnboundedReceiverStream;

/// Gossip defaults to 4 KiB, and a Share carries a whole buffer.
///
/// Gossip allocates each frame at its actual size, so a high limit costs
/// nothing up front.
const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

/// What the node tells the editor. Polled by `Editor::wait_event`.
#[derive(Debug)]
pub enum Event {
    /// We connected directly to another member.
    ///
    /// Gossip only reports direct neighbours, not every member of the swarm,
    /// so a member can be in the session without us ever seeing it here.
    NeighborUp(EndpointId),
    /// Some member broadcast these bytes. They may have been relayed by other
    /// members on the way, so there is no reliable sender to report.
    Received(Bytes),
    /// The node left the session by itself, for the given reason.
    Closed(String),
    /// A request failed. The node is still usable.
    Error(String),
}

/// What the editor asks of the node. Private, so callers go through the
/// [`Service`] methods instead of building requests themselves.
#[derive(Debug)]
enum Request {
    /// Carries the channel to reply on, as the ticket depends on node state.
    Ticket(oneshot::Sender<String>),
    Join(String),
    Close,
    Broadcast(Bytes),
}

/// Handle to the actor task that owns the Node.
///
/// Cheap to clone, so hooks can keep their own copy to broadcast from.
#[derive(Clone)]
pub struct Service {
    id: EndpointId,
    requests: UnboundedSender<Request>,
}

impl Service {
    /// Starts the node, returning a handle to it and the stream of its events.
    pub fn new() -> (Self, UnboundedReceiverStream<Event>) {
        let (events_tx, events_rx) = unbounded_channel();
        let (requests_tx, mut requests_rx) = unbounded_channel();

        // Generated here rather than by the endpoint, so the id is known
        // right away, before the endpoint has finished binding.
        let secret_key = SecretKey::generate();
        let id = secret_key.public();

        // Binding is async, and Editor::new is not, so the whole setup runs
        // inside the task instead of before it.

        tokio::spawn(async move {
            // N0 uses n0's public relays and address lookup, so peers can
            // reach each other behind NATs without any configuration.
            let endpoint = Endpoint::builder(presets::N0)
                .secret_key(secret_key)
                .bind()
                .await
                .expect("failed to bind the endpoint");
            // Wait for a relay connection, so the address we put in tickets
            // is one peers can actually reach.
            endpoint.online().await;
            log::info!("listening as {}", endpoint.id().fmt_short());

            let gossip = Gossip::builder()
                .max_message_size(MAX_MESSAGE_SIZE)
                .spawn(endpoint.clone());
            // The router accepts incoming connections and hands the ones
            // speaking gossip's ALPN to gossip. It is kept in a variable
            // because dropping it would stop accepting connections.
            let _router = Router::builder(endpoint.clone())
                .accept(ALPN, gossip.clone())
                .spawn();

            let mut node = Node::new(endpoint, gossip, events_tx);

            // Requests from the editor and events from the topic are handled
            // in the same loop, so only this task ever touches the node.
            loop {
                tokio::select! {
                    request = requests_rx.recv() => {
                        // Every Service handle is gone, so the editor is too.
                        let Some(request) = request else {
                            break;
                        };
                        node.handle(request).await;
                    }
                    event = node.next_event() => node.on_event(event),
                }
            }
        });

        let service = Service {
            id,
            requests: requests_tx,
        };
        (service, UnboundedReceiverStream::new(events_rx))
    }

    pub fn id(&self) -> EndpointId {
        self.id
    }

    /// Resolves to a ticket into the current session, starting one if needed.
    ///
    /// The future does not borrow the service, so it can run as a job, which
    /// has to be `'static`.
    pub fn ticket(&self) -> impl Future<Output = String> + 'static {
        let (tx, rx) = oneshot::channel();
        self.send(Request::Ticket(tx));
        async move { rx.await.expect("node should reply with a ticket") }
    }

    /// Failures come back as an [`Event::Error`], as the node handles the
    /// request after this returns.
    pub fn join(&self, ticket: String) {
        self.send(Request::Join(ticket));
    }

    /// Leaves the session. Unlike the node leaving by itself, this emits no
    /// [`Event::Closed`], since the caller already knows.
    pub fn close(&self) {
        self.send(Request::Close);
    }

    /// Sends to every member of the session. Delivery is best-effort, and
    /// outside a session the message is silently dropped.
    pub fn broadcast(&self, message: Bytes) {
        self.send(Request::Broadcast(message));
    }

    /// The node only stops once every handle is dropped, or if it panicked,
    /// for instance because the endpoint failed to bind.
    fn send(&self, request: Request) {
        self.requests
            .send(request)
            .expect("p2p node should be running");
    }
}

/// An invitation into a session: its topic and one member to dial.
///
/// Any member can hand one out, since it only needs to name some member to
/// connect to first. Gossip takes care of meeting the rest of the swarm.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTicket {
    pub topic: TopicId,
    pub addr: EndpointAddr,
}

// Ticket gives us a copy-pasteable string format, prefixed with KIND.
impl Ticket for SessionTicket {
    const KIND: &'static str = "helix";

    fn encode_bytes(&self) -> Vec<u8> {
        postcard::to_stdvec(self).expect("ticket should serialize")
    }

    fn decode_bytes(bytes: &[u8]) -> Result<Self, ParseError> {
        Ok(postcard::from_bytes(bytes)?)
    }
}

/// Our endpoint in the session's gossip swarm.
struct Node {
    endpoint: Endpoint,
    gossip: Gossip,
    /// Addresses learned from tickets for gossip to dial,
    /// as it dials bootstrap peers by id alone.
    addresses: MemoryLookup,
    events: UnboundedSender<Event>,
    /// The swarm we are in, if any. We are in at most one session at a time.
    ///
    /// Dropping the GossipTopic makes gossip leave the topic.
    topic: Option<(TopicId, GossipTopic)>,
}

impl Node {
    fn new(endpoint: Endpoint, gossip: Gossip, events: UnboundedSender<Event>) -> Self {
        // Registered once, and filled in as we join sessions.
        let addresses = MemoryLookup::new();
        endpoint
            .address_lookup()
            .expect("endpoint should be open")
            .add(addresses.clone());

        Self {
            endpoint,
            gossip,
            addresses,
            events,
            topic: None,
        }
    }

    async fn handle(&mut self, request: Request) {
        let result = match request {
            Request::Ticket(chan) => {
                let _ = chan.send(self.ticket().await);
                Ok(())
            }
            Request::Join(ticket) => self.join(&ticket).await,
            Request::Close => {
                self.close();
                Ok(())
            }
            Request::Broadcast(message) => self.broadcast(message).await,
        };

        // Nobody awaits the requests, so errors can only go out as events.
        if let Err(err) = result {
            self.report(format!("{:#}", err));
        }
    }

    async fn ticket(&mut self) -> String {
        let topic = match &self.topic {
            // Already in a session, so the ticket invites into it.
            Some((topic, _)) => *topic,
            // Otherwise start one. A session is just a gossip topic, and a
            // random id keeps it from colliding with anyone else's.
            None => {
                let topic = TopicId::from_bytes(rand::random());
                let subscription = self
                    .gossip
                    // No bootstrap peers: we are the first member.
                    .subscribe(topic, Vec::new())
                    .await
                    .expect("gossip should be running");
                self.topic = Some((topic, subscription));
                topic
            }
        };

        SessionTicket {
            topic,
            // Includes our relay and direct addresses, so the joiner can
            // dial us without looking anything up.
            addr: self.endpoint.addr(),
        }
        .encode_string()
    }

    async fn join(&mut self, ticket: &str) -> Result<()> {
        let SessionTicket { topic, addr } = SessionTicket::decode_string(ticket)?;

        ensure!(
            addr.id != self.endpoint.id(),
            "cannot join your own session"
        );
        ensure!(
            self.topic.is_none(),
            "already in a session, close it before joining another"
        );

        // Gossip dials bootstrap peers by id alone, so tell the endpoint
        // where that id lives first.
        let bootstrap = addr.id;
        self.addresses.add_endpoint_info(addr);
        // Does not wait for the bootstrap peer. Once we connect, the topic
        // yields a NeighborUp.
        let subscription = self.gossip.subscribe(topic, vec![bootstrap]).await?;
        self.topic = Some((topic, subscription));
        Ok(())
    }

    async fn broadcast(&mut self, message: Bytes) -> Result<()> {
        // Not an error: a buffer can be shared before any session exists,
        // and it is offered again to each peer that connects later.
        let Some((_, subscription)) = &mut self.topic else {
            return Ok(());
        };

        // Only best-effort delivery. A peer that is not connected right now
        // never gets this message.
        subscription.broadcast(message).await?;
        Ok(())
    }

    fn close(&mut self) {
        self.topic = None;
    }

    async fn next_event(&mut self) -> Option<Result<GossipEvent, ApiError>> {
        match &mut self.topic {
            // Without a topic there is nothing to wait for, and pending keeps
            // the select loop waiting on requests alone.
            Some((_, subscription)) => subscription.next().await,
            None => std::future::pending().await,
        }
    }

    fn on_event(&mut self, event: Option<Result<GossipEvent, ApiError>>) {
        match event {
            Some(Ok(GossipEvent::NeighborUp(id))) => {
                log::info!("connected to {}", id.fmt_short());
                let _ = self.events.send(Event::NeighborUp(id));
            }
            // Gossip reconnects and repairs the swarm by itself, and nothing
            // in the session depends on who is connected, so only log it.
            Some(Ok(GossipEvent::NeighborDown(id))) => {
                log::info!("disconnected from {}", id.fmt_short());
            }
            Some(Ok(GossipEvent::Received(message))) => {
                let _ = self.events.send(Event::Received(message.content));
            }
            // We lost some messages. The dropped edits are lost for good, so
            // leave instead of drifting apart unnoticed. Gossip closes a
            // lagging subscription anyway.
            Some(Ok(GossipEvent::Lagged)) => {
                self.drop_topic("fell behind the session and left it".into())
            }
            Some(Err(err)) => self.drop_topic(format!("session failed: {:#}", err)),
            // The topic's stream ended, so gossip is done with it.
            None => self.drop_topic("left the session".into()),
        }
    }

    /// Leaves the session on our own, unlike [`Request::Close`], so the
    /// editor has to hear about it.
    fn drop_topic(&mut self, reason: String) {
        log::error!("{reason}");
        self.close();
        let _ = self.events.send(Event::Closed(reason));
    }

    fn report(&self, error: String) {
        log::error!("{error}");
        let _ = self.events.send(Event::Error(error));
    }
}
