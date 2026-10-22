//! The transport layer moves opaque bytes between peers.

use std::{collections::HashMap, future::Future};

use anyhow::{ensure, Result};
use bytes::Bytes;
pub use iroh::EndpointId;
use iroh::{
    address_lookup::memory::MemoryLookup,
    endpoint::{presets, Connection},
    protocol::{AcceptError, ProtocolHandler, Router},
    Endpoint, EndpointAddr, SecretKey,
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

const REQUEST_ALPN: &[u8] = b"helix/request/0";

/// A gossip swarm we can be in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Channel {
    Session,
    Document(TopicId),
}

/// What the service tells the editor.
#[derive(Debug)]
pub enum Event {
    /// A message broadcast to a channel.
    Received(Channel, Bytes),
    /// A peer's request, to answer through the sender.
    Request(Bytes, oneshot::Sender<Result<Bytes>>),
    /// The response to our request, along with the request.
    Response(Bytes, Result<Bytes>),
    Exit(Channel, String),
}

/// What the editor asks to the service.
#[derive(Debug)]
enum Request {
    Ticket(oneshot::Sender<String>),
    Join(String, oneshot::Sender<Result<()>>),
    Subscribe(TopicId, Vec<EndpointId>),
    Unsubscribe(TopicId),
    Broadcast(Channel, Bytes),
    Send(EndpointId, Bytes),
    Shutdown,
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

    pub fn ticket(&self) -> impl Future<Output = String> + 'static {
        let (tx, rx) = oneshot::channel();
        self.send(Request::Ticket(tx));
        async move { rx.await.expect("actor should reply") }
    }

    pub fn join(&self, ticket: String) -> impl Future<Output = Result<()>> + 'static {
        let (tx, rx) = oneshot::channel();
        self.send(Request::Join(ticket, tx));
        async move { rx.await.expect("actor should reply") }
    }

    pub fn shutdown(&self) {
        self.send(Request::Shutdown);
    }

    pub fn subscribe(&self, topic: TopicId, bootstrap: Vec<EndpointId>) {
        self.send(Request::Subscribe(topic, bootstrap));
    }

    pub fn unsubscribe(&self, topic: TopicId) {
        self.send(Request::Unsubscribe(topic));
    }

    pub fn broadcast(&self, channel: Channel, message: Bytes) {
        self.send(Request::Broadcast(channel, message));
    }

    pub fn request(&self, peer: EndpointId, message: Bytes) {
        self.send(Request::Send(peer, message));
    }

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

type Events = stream::Boxed<Option<Result<GossipEvent, ApiError>>>;

struct Actor {
    endpoint: Endpoint,
    _router: Router,
    gossip: Gossip,
    /// Addresses learned from tickets.
    addresses: MemoryLookup,
    events: UnboundedSender<Event>,
    /// Current session, if one.
    session: Option<TopicId>,
    senders: HashMap<Channel, GossipSender>,
    receivers: StreamMap<Channel, Events>,
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
        let requests = RequestProtocol {
            events: events.clone(),
        };
        let router = Router::builder(endpoint.clone())
            .accept(ALPN, gossip.clone())
            .accept(REQUEST_ALPN, requests)
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

    async fn run(mut self, mut requests: UnboundedReceiver<Request>) {
        loop {
            tokio::select! {
                request = requests.recv() => {
                    // If every service handle is gone, we exit.
                    let Some(request) = request else {
                        break;
                    };
                    self.handle(request).await;
                }
                Some((channel, event)) = self.receivers.next() => self.on_event(channel, event),
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
                let _ = chan.send(result);
            }
            Request::Subscribe(id, bootstrap) => self.subscribe_document(id, bootstrap).await,
            Request::Unsubscribe(id) => self.unsubscribe(Channel::Document(id)),
            Request::Broadcast(channel, message) => {
                if let Err(err) = self.broadcast(channel, message).await {
                    log::warn!("failed to broadcast: {err:#}")
                }
            }
            Request::Send(peer, message) => self.request(peer, message),
            Request::Shutdown => self.shutdown(),
        }
    }

    fn request(&self, peer: EndpointId, message: Bytes) {
        log::trace!(
            "sending {} bytes request to {}",
            message.len(),
            peer.fmt_short()
        );
        let endpoint = self.endpoint.clone();
        let events = self.events.clone();
        tokio::spawn(async move {
            let response = async {
                let connection = endpoint.connect(peer, REQUEST_ALPN).await?;
                let (mut send, mut recv) = connection.open_bi().await?;
                send.write_all(&message).await?;
                send.finish()?;
                let response = recv.read_to_end(MAX_MESSAGE_SIZE).await?;
                // We close once we have read everything, since the peer waits for it.
                connection.close(0u32.into(), b"done");
                anyhow::Ok(Bytes::from(response))
            }
            .await;
            let _ = events.send(Event::Response(message, response));
        });
    }

    async fn ticket(&mut self) -> String {
        let topic = match self.session {
            Some(topic) => topic,
            None => {
                let topic = TopicId::from_bytes(rand::random());
                let subscription = self
                    .gossip
                    .subscribe(topic, Vec::new())
                    .await
                    .expect("gossip should be running");
                self.add_subscription(Channel::Session, subscription);
                self.session = Some(topic);
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
        let subscription = self
            .gossip
            .subscribe_and_join(topic, vec![bootstrap])
            .await?;
        self.add_subscription(Channel::Session, subscription);
        self.session = Some(topic);
        Ok(())
    }

    async fn subscribe_document(&mut self, id: TopicId, bootstrap: Vec<EndpointId>) {
        log::info!("subscribing to document {}", id.fmt_short());
        let subscription = if bootstrap.is_empty() {
            self.gossip.subscribe(id, bootstrap).await
        } else {
            // Wait until connected to a peer. This can stall the actor.
            self.gossip.subscribe_and_join(id, bootstrap).await
        }
        .expect("gossip should be running");
        self.add_subscription(Channel::Document(id), subscription);
    }

    fn add_subscription(&mut self, channel: Channel, subscription: GossipTopic) {
        let (sender, receiver) = subscription.split();
        let events = receiver.map(Some).chain(stream::once(None)).boxed();
        self.senders.insert(channel, sender);
        self.receivers.insert(channel, events);
    }

    fn unsubscribe(&mut self, channel: Channel) {
        self.senders.remove(&channel);
        self.receivers.remove(&channel);
        log::info!("unsubscribed from channel {channel:?}");
    }

    async fn broadcast(&mut self, channel: Channel, message: Bytes) -> Result<()> {
        let Some(sender) = self.senders.get(&channel) else {
            return Ok(());
        };

        log::trace!("broadcasting {} bytes to {channel:?}", message.len());
        sender.broadcast(message).await?;
        Ok(())
    }

    fn shutdown(&mut self) {
        self.senders.clear();
        self.receivers.clear();
        self.session.take();
        log::info!("shutdown session");
    }

    fn on_event(&mut self, channel: Channel, event: Option<Result<GossipEvent, ApiError>>) {
        match event {
            Some(Ok(GossipEvent::NeighborUp(id))) => {
                log::info!("connected to {} in {channel:?}", id.fmt_short());
            }
            Some(Ok(GossipEvent::NeighborDown(id))) => {
                log::info!("disconnected from {} in {channel:?}", id.fmt_short());
            }
            Some(Ok(GossipEvent::Received(message))) => {
                log::trace!(
                    "received {} bytes from {} in {channel:?}",
                    message.content.len(),
                    message.delivered_from.fmt_short()
                );
                let _ = self.events.send(Event::Received(channel, message.content));
            }
            Some(Ok(GossipEvent::Lagged)) => self.exit(channel, "fell behind".into()),
            Some(Err(err)) => self.exit(channel, format!("gossip failed: {err:#}")),
            None => self.exit(channel, "gossip stream ended".into()),
        }
    }

    fn exit(&mut self, channel: Channel, reason: String) {
        if channel == Channel::Session {
            self.shutdown();
        } else {
            self.unsubscribe(channel);
        }
        let _ = self.events.send(Event::Exit(channel, reason));
    }
}

/// Protocol for request/response communication between peers.
#[derive(Debug, Clone)]
struct RequestProtocol {
    events: UnboundedSender<Event>,
}

impl ProtocolHandler for RequestProtocol {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let (mut send, mut recv) = connection.accept_bi().await?;
        let request = recv
            .read_to_end(MAX_MESSAGE_SIZE)
            .await
            .map_err(AcceptError::from_err)?;

        let (reply, response) = oneshot::channel();
        let _ = self.events.send(Event::Request(request.into(), reply));
        let response = match response.await.map_err(AcceptError::from_err)? {
            Ok(response) => response,
            Err(err) => {
                log::warn!("refusing request: {err:#}");
                connection.close(1u32.into(), format!("{err:#}").as_bytes());
                return Ok(());
            }
        };

        send.write_all(&response)
            .await
            .map_err(AcceptError::from_err)?;
        send.finish()?;
        connection.closed().await;
        Ok(())
    }
}
