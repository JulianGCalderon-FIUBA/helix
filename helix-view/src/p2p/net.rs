//! The transport layer moves opaque bytes between peers.

use std::{collections::HashMap, future::Future};

use anyhow::{ensure, Result};
use bytes::Bytes;
pub use iroh::EndpointId;
use iroh::{
    address_lookup::memory::MemoryLookup, endpoint::presets, protocol::Router, Endpoint,
    EndpointAddr, SecretKey,
};
pub use iroh_gossip::TopicId;
use iroh_gossip::{
    api::{ApiError, Event as GossipEvent, GossipSender, GossipTopic},
    Gossip, ALPN,
};
use iroh_tickets::{ParseError, Ticket};
use n0_future::{stream, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::{
    mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
    oneshot,
};
use tokio_stream::{wrappers::UnboundedReceiverStream, StreamMap};

const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

/// A gossip swarm we can be in.
///
/// The session topic is the one tickets invite into. Other topics are
/// joined by id, and only by the peers that care about them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Topic {
    Session,
    Other(TopicId),
}

/// What the service tells the editor.
#[derive(Debug)]
pub enum Event {
    NeighborUp(Topic, EndpointId),
    Received(Topic, Bytes),
    Quit(Topic, String),
}

/// What the editor asks to the service.
#[derive(Debug)]
enum Request {
    Ticket(oneshot::Sender<String>),
    Join(String, oneshot::Sender<Result<()>>),
    Leave,
    Subscribe(TopicId, Vec<EndpointId>),
    Unsubscribe(TopicId),
    Broadcast(Topic, Bytes),
}

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

    /// Creates a ticket into the current session, starting one if needed.
    pub fn ticket(&self) -> impl Future<Output = String> + 'static {
        let (tx, rx) = oneshot::channel();
        self.send(Request::Ticket(tx));
        async move { rx.await.expect("actor should reply with a ticket") }
    }

    /// Joins a session.
    pub fn join(&self, ticket: String) -> impl Future<Output = Result<()>> + 'static {
        let (tx, rx) = oneshot::channel();
        self.send(Request::Join(ticket, tx));
        async move { rx.await.expect("actor should reply") }
    }

    /// Leaves the session.
    pub fn leave(&self) {
        self.send(Request::Leave);
    }

    /// Joins a topic. We get a NeighborUp once we connect to a peer there.
    pub fn subscribe(&self, topic: TopicId, bootstrap: Vec<EndpointId>) {
        self.send(Request::Subscribe(topic, bootstrap));
    }

    pub fn unsubscribe(&self, topic: TopicId) {
        self.send(Request::Unsubscribe(topic));
    }

    /// Sends to every member of a topic. Delivery is best-effort.
    pub fn broadcast(&self, topic: Topic, message: Bytes) {
        self.send(Request::Broadcast(topic, message));
    }

    /// Sends a request to the internal actor.
    fn send(&self, request: Request) {
        self.requests
            .send(request)
            .expect("internal actor should be running");
    }
}

/// An invitation into a session. Any member can hand one out.
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

/// A topic's events, followed by a final `None` once the subscription
/// ends. StreamMap drops finished streams silently, and we want to know.
type Events = stream::Boxed<Option<Result<GossipEvent, ApiError>>>;

/// The Service's internal actor
struct Actor {
    endpoint: Endpoint,
    _router: Router,
    gossip: Gossip,
    /// Addresses learned from tickets.
    addresses: MemoryLookup,
    events: UnboundedSender<Event>,
    /// The session's topic id, if we are in one.
    session: Option<TopicId>,
    /// Every topic we are in, including the session's.
    senders: HashMap<Topic, GossipSender>,
    receivers: StreamMap<Topic, Events>,
}

impl Actor {
    async fn bind(secret_key: SecretKey, events: UnboundedSender<Event>) -> Self {
        let endpoint = Endpoint::builder(presets::N0)
            .secret_key(secret_key)
            .bind()
            .await
            .expect("failed to bind the endpoint");
        endpoint.online().await;
        log::info!("listening as {}", endpoint.id().fmt_short());

        let gossip = Gossip::builder()
            .max_message_size(MAX_MESSAGE_SIZE)
            .spawn(endpoint.clone());
        let router = Router::builder(endpoint.clone())
            .accept(ALPN, gossip.clone())
            .spawn();

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
            session: None,
            senders: HashMap::new(),
            receivers: StreamMap::new(),
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
                Some((topic, event)) = self.receivers.next() => self.on_event(topic, event),
            }
        }
    }

    async fn handle(&mut self, request: Request) {
        match request {
            Request::Ticket(chan) => {
                let _ = chan.send(self.ticket().await);
            }
            Request::Join(ticket, chan) => {
                let result = self.join(&ticket).await;
                if let Err(err) = &result {
                    log::error!("failed to join session: {err:#}");
                }
                let _ = chan.send(result);
            }
            Request::Leave => self.leave(),
            Request::Subscribe(topic, bootstrap) => self.subscribe_topic(topic, bootstrap).await,
            Request::Unsubscribe(topic) => self.unsubscribe(Topic::Other(topic)),
            Request::Broadcast(topic, message) => {
                if let Err(err) = self.broadcast(topic, message).await {
                    log::warn!("failed to broadcast: {err:#}");
                }
            }
        }
    }

    async fn ticket(&mut self) -> String {
        let topic = match self.session {
            Some(topic) => topic,
            None => {
                let topic = TopicId::from_bytes(rand::random());
                self.subscribe_session(topic, Vec::new()).await;
                log::info!("created session {}", topic.fmt_short());
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
        ensure!(self.session.is_none(), "already in a session");

        let bootstrap = addr.id;
        self.addresses.add_endpoint_info(addr);
        log::info!(
            "joining session {} through {}",
            topic.fmt_short(),
            bootstrap.fmt_short()
        );
        self.subscribe_session(topic, vec![bootstrap]).await;
        Ok(())
    }

    async fn subscribe_session(&mut self, id: TopicId, bootstrap: Vec<EndpointId>) {
        let subscription = self
            .gossip
            .subscribe(id, bootstrap)
            .await
            .expect("gossip should be running");
        self.session = Some(id);
        self.insert(Topic::Session, subscription);
    }

    async fn subscribe_topic(&mut self, id: TopicId, bootstrap: Vec<EndpointId>) {
        let subscription = self
            .gossip
            .subscribe(id, bootstrap)
            .await
            .expect("gossip should be running");
        self.insert(Topic::Other(id), subscription);
    }

    fn insert(&mut self, topic: Topic, subscription: GossipTopic) {
        let (sender, receiver) = subscription.split();
        let events = receiver.map(Some).chain(stream::once(None)).boxed();
        self.senders.insert(topic, sender);
        self.receivers.insert(topic, events);
    }

    /// Dropping both halves of a subscription leaves the topic.
    fn unsubscribe(&mut self, topic: Topic) {
        self.senders.remove(&topic);
        self.receivers.remove(&topic);
    }

    async fn broadcast(&mut self, topic: Topic, message: Bytes) -> Result<()> {
        let Some(sender) = self.senders.get(&topic) else {
            return Ok(());
        };

        log::trace!("broadcasting {} bytes to {topic:?}", message.len());
        sender.broadcast(message).await?;
        Ok(())
    }

    fn leave(&mut self) {
        self.senders.clear();
        self.receivers.clear();
        if let Some(topic) = self.session.take() {
            log::info!("left session {}", topic.fmt_short());
        }
    }

    fn on_event(&mut self, topic: Topic, event: Option<Result<GossipEvent, ApiError>>) {
        match event {
            Some(Ok(GossipEvent::NeighborUp(id))) => {
                log::info!("connected to {} in {topic:?}", id.fmt_short());
                let _ = self.events.send(Event::NeighborUp(topic, id));
            }
            Some(Ok(GossipEvent::NeighborDown(id))) => {
                log::info!("disconnected from {} in {topic:?}", id.fmt_short());
            }
            Some(Ok(GossipEvent::Received(message))) => {
                log::trace!(
                    "received {} bytes from {} in {topic:?}",
                    message.content.len(),
                    message.delivered_from.fmt_short()
                );
                let _ = self.events.send(Event::Received(topic, message.content));
            }
            Some(Ok(GossipEvent::Lagged)) => self.quit(topic, "fell behind".into()),
            Some(Err(err)) => self.quit(topic, format!("gossip failed: {err:#}")),
            None => self.quit(topic, "gossip stream ended".into()),
        }
    }

    /// Losing the session topic means losing the whole session.
    fn quit(&mut self, topic: Topic, reason: String) {
        if topic == Topic::Session {
            self.leave();
        } else {
            self.unsubscribe(topic);
        }
        log::error!("quit {topic:?}: {reason}");
        let _ = self.events.send(Event::Quit(topic, reason));
    }
}
