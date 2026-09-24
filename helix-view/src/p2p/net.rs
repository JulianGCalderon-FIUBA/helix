//! The transport layer moves opaque bytes between the members of a session and
//! knows nothing about documents.

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
    mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
    oneshot,
};
use tokio_stream::wrappers::UnboundedReceiverStream;

const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

/// What the service tells the editor.
#[derive(Debug)]
pub enum Event {
    NeighborUp(EndpointId),
    /// Some member broadcast these bytes.
    Received(Bytes),
    Closed(String),
    Error(String),
}

/// What the editor asks of the actor.
#[derive(Debug)]
enum Request {
    Ticket(oneshot::Sender<String>),
    Join(String),
    Close,
    Broadcast(Bytes),
}

/// Handle to the actor task that owns the service.
#[derive(Clone)]
pub struct Service {
    id: EndpointId,
    requests: UnboundedSender<Request>,
}

impl Service {
    pub fn new() -> (Self, UnboundedReceiverStream<Event>) {
        let (events_tx, events_rx) = unbounded_channel();
        let (requests_tx, requests_rx) = unbounded_channel();

        let secret_key = SecretKey::generate();
        let id = secret_key.public();

        tokio::spawn(async move {
            Actor::bind(secret_key, events_tx)
                .await
                .run(requests_rx)
                .await;
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
    pub fn ticket(&self) -> impl Future<Output = String> + 'static {
        let (tx, rx) = oneshot::channel();
        self.send(Request::Ticket(tx));
        async move { rx.await.expect("actor should reply with a ticket") }
    }

    /// Attempts to join a session.
    pub fn join(&self, ticket: String) {
        self.send(Request::Join(ticket));
    }

    /// Leaves the session. Emits no Closed event.
    pub fn close(&self) {
        self.send(Request::Close);
    }

    /// Sends to every member of the session. Delivery is best-effort.
    pub fn broadcast(&self, message: Bytes) {
        self.send(Request::Broadcast(message));
    }

    /// Sends a request to the internal actor.
    fn send(&self, request: Request) {
        self.requests
            .send(request)
            .expect("p2p actor should be running");
    }
}

/// An invitation into a session: its topic and one member to dial. Any member
/// can hand one out, since it only needs to name some member to connect to
/// first.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTicket {
    pub topic: TopicId,
    pub addr: EndpointAddr,
}

impl Ticket for SessionTicket {
    const KIND: &'static str = "helix";

    fn encode_bytes(&self) -> Vec<u8> {
        postcard::to_stdvec(self).expect("ticket should serialize")
    }

    fn decode_bytes(bytes: &[u8]) -> Result<Self, ParseError> {
        Ok(postcard::from_bytes(bytes)?)
    }
}

/// The [`Service`]'s internal actor, owning our endpoint and the session's
/// gossip topic. Runs in its own task, so none of this needs locking.
struct Actor {
    endpoint: Endpoint,
    gossip: Gossip,
    /// Accepts incoming connections and hands the ones speaking gossip's ALPN
    /// to gossip. Kept alive by this field: dropping it stops accepting.
    _router: Router,
    /// Addresses learned from tickets for gossip to dial,
    /// as it dials bootstrap peers by id alone.
    addresses: MemoryLookup,
    events: UnboundedSender<Event>,
    /// The swarm we are in, if any.
    topic: Option<(TopicId, GossipTopic)>,
}

impl Actor {
    /// Binds the endpoint and starts gossip on it.
    async fn bind(secret_key: SecretKey, events: UnboundedSender<Event>) -> Self {
        // N0 uses n0's public relays and address lookup, so peers can reach
        // each other behind NATs without any configuration.
        let endpoint = Endpoint::builder(presets::N0)
            .secret_key(secret_key)
            .bind()
            .await
            .expect("failed to bind the endpoint");
        // Wait for a relay connection, so the address we put in tickets is
        // one peers can actually reach.
        endpoint.online().await;
        log::info!("listening as {}", endpoint.id().fmt_short());

        let gossip = Gossip::builder()
            .max_message_size(MAX_MESSAGE_SIZE)
            .spawn(endpoint.clone());
        let router = Router::builder(endpoint.clone())
            .accept(ALPN, gossip.clone())
            .spawn();

        // Registered once, and filled in as we join sessions.
        let addresses = MemoryLookup::new();
        endpoint
            .address_lookup()
            .expect("endpoint should be open")
            .add(addresses.clone());

        Self {
            endpoint,
            gossip,
            _router: router,
            addresses,
            events,
            topic: None,
        }
    }

    /// Handles requests and gossip events until the editor is gone.
    async fn run(mut self, mut requests: UnboundedReceiver<Request>) {
        loop {
            tokio::select! {
                request = requests.recv() => {
                    // Every Service handle is gone, so the editor is too.
                    let Some(request) = request else {
                        break;
                    };
                    self.handle(request).await;
                }
                event = self.next_event() => self.on_event(event),
            }
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
            Some((topic, _)) => *topic,
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

        let bootstrap = addr.id;
        self.addresses.add_endpoint_info(addr);
        let subscription = self.gossip.subscribe(topic, vec![bootstrap]).await?;
        self.topic = Some((topic, subscription));
        Ok(())
    }

    async fn broadcast(&mut self, message: Bytes) -> Result<()> {
        let Some((_, subscription)) = &mut self.topic else {
            return Ok(());
        };

        subscription.broadcast(message).await?;
        Ok(())
    }

    fn close(&mut self) {
        self.topic = None;
    }

    async fn next_event(&mut self) -> Option<Result<GossipEvent, ApiError>> {
        match &mut self.topic {
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
