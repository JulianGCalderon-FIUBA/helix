//! A topic based transport layer that moves opaque bytes between peers.
//! It knows nothing about collaboration.

use std::{collections::HashMap, fmt, future::Future};

use anyhow::Result;
use bytes::Bytes;
use iroh::{
    address_lookup::memory::MemoryLookup,
    endpoint::{presets, Connection},
    protocol::{AcceptError, ProtocolHandler, Router},
    Endpoint, SecretKey,
};
pub use iroh::{EndpointAddr, EndpointId};
pub use iroh_gossip::TopicId;
use iroh_gossip::{
    api::{ApiError, Event as GossipEvent, GossipSender, GossipTopic},
    Gossip, ALPN,
};
use n0_future::{stream, StreamExt};
use tokio::sync::{
    mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
    oneshot,
};
use tokio_stream::{wrappers::UnboundedReceiverStream, StreamMap};

const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

const PEER_ALPN: &[u8] = b"helix/peer/0";

/// Where messages go to and come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Channel {
    /// A gossip swarm.
    Topic(TopicId),
    /// A direct connection to a single peer.
    Peer(EndpointId),
}

impl fmt::Display for Channel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Channel::Topic(topic) => write!(f, "topic {}", topic.fmt_short()),
            Channel::Peer(peer) => write!(f, "peer {}", peer.fmt_short()),
        }
    }
}

/// What the service tells the editor.
#[derive(Debug)]
pub enum Event {
    Received(Channel, Bytes),
    Exit(Channel),
}

/// What the editor asks to the service.
#[derive(Debug)]
enum Request {
    Address(oneshot::Sender<EndpointAddr>),
    AddAddress(EndpointAddr),
    Subscribe(TopicId, Vec<EndpointId>, oneshot::Sender<Result<()>>),
    Unsubscribe(TopicId),
    Send(Channel, Bytes),
    Reset,
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

    pub fn address(&self) -> impl Future<Output = EndpointAddr> + 'static {
        let (tx, rx) = oneshot::channel();
        self.request(Request::Address(tx));
        async move { rx.await.expect("actor should reply") }
    }

    /// Lets us reach a peer we haven't met through gossip.
    pub fn add_address(&self, addr: EndpointAddr) {
        self.request(Request::AddAddress(addr));
    }

    /// The request is sent right away, so the future can be dropped.
    pub fn subscribe(
        &self,
        topic: TopicId,
        bootstrap: Vec<EndpointId>,
    ) -> impl Future<Output = Result<()>> + 'static {
        let (tx, rx) = oneshot::channel();
        self.request(Request::Subscribe(topic, bootstrap, tx));
        async move { rx.await.expect("actor should reply") }
    }

    pub fn reset(&self) {
        self.request(Request::Reset);
    }

    pub fn unsubscribe(&self, topic: TopicId) {
        self.request(Request::Unsubscribe(topic));
    }

    pub fn send(&self, channel: Channel, message: Bytes) {
        self.request(Request::Send(channel, message));
    }

    fn request(&self, request: Request) {
        self.requests
            .send(request)
            .expect("internal actor should be running");
    }
}

type Events = stream::Boxed<Option<Result<GossipEvent, ApiError>>>;
type PeerMessages = stream::Boxed<Result<Bytes>>;

struct Actor {
    endpoint: Endpoint,
    _router: Router,
    gossip: Gossip,
    /// Addresses we were given.
    addresses: MemoryLookup,
    events: UnboundedSender<Event>,
    senders: HashMap<Channel, GossipSender>,
    receivers: StreamMap<Channel, Events>,
    /// Connections that peers opened to us.
    incoming: UnboundedReceiver<Connection>,
    peers: HashMap<EndpointId, Connection>,
    peer_messages: StreamMap<EndpointId, PeerMessages>,
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
        let (incoming_tx, incoming) = unbounded_channel();
        let router = Router::builder(endpoint.clone())
            .accept(ALPN, gossip.clone())
            .accept(
                PEER_ALPN,
                PeerProtocol {
                    connections: incoming_tx,
                },
            )
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
            senders: HashMap::new(),
            receivers: StreamMap::new(),
            incoming,
            peers: HashMap::new(),
            peer_messages: StreamMap::new(),
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
                Some(connection) = self.incoming.recv() => self.add_peer(connection),
                Some((peer, message)) = self.peer_messages.next() => {
                    self.on_peer_message(peer, message)
                }
            }
        }
    }

    async fn handle(&mut self, request: Request) {
        match request {
            Request::Address(chan) => {
                let _ = chan.send(self.endpoint.addr());
            }
            Request::AddAddress(addr) => self.addresses.add_endpoint_info(addr),
            Request::Subscribe(topic, bootstrap, chan) => {
                let result = self.subscribe(topic, bootstrap).await;
                if let Err(err) = &result {
                    self.exit(
                        Channel::Topic(topic),
                        format!("failed to subscribe: {err:#}"),
                    );
                }
                let _ = chan.send(result);
            }
            Request::Unsubscribe(topic) => self.unsubscribe(Channel::Topic(topic)),
            Request::Send(Channel::Peer(peer), message) => {
                if let Err(err) = self.send_to_peer(peer, message).await {
                    self.exit(Channel::Peer(peer), format!("failed to send: {err:#}"));
                }
            }
            Request::Send(channel, message) => {
                if let Err(err) = self.broadcast(channel, message).await {
                    log::warn!("failed to broadcast: {err:#}")
                }
            }
            Request::Reset => self.reset(),
        }
    }

    async fn subscribe(&mut self, topic: TopicId, bootstrap: Vec<EndpointId>) -> Result<()> {
        log::debug!("subscribing to topic {}", topic.fmt_short());
        let subscription = if bootstrap.is_empty() {
            self.gossip.subscribe(topic, bootstrap).await?
        } else {
            // Wait until connected to a peer. This can stall the actor.
            self.gossip.subscribe_and_join(topic, bootstrap).await?
        };
        self.add_subscription(Channel::Topic(topic), subscription);
        Ok(())
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
        log::info!("unsubscribed from {channel}");
    }

    async fn send_to_peer(&mut self, peer: EndpointId, message: Bytes) -> Result<()> {
        let connection = match self.peers.get(&peer) {
            Some(connection) => connection.clone(),
            None => {
                // Wait until connected. This can stall the actor.
                let connection = self.endpoint.connect(peer, PEER_ALPN).await?;
                self.add_peer(connection.clone());
                connection
            }
        };

        log::trace!("sending {} bytes to {}", message.len(), peer.fmt_short());
        let mut send = connection.open_uni().await?;
        send.write_all(&message).await?;
        send.finish()?;
        Ok(())
    }

    fn add_peer(&mut self, connection: Connection) {
        let peer = connection.remote_id();
        log::info!("connected to peer {}", peer.fmt_short());
        // Each message comes in its own stream, so it needs no framing.
        let messages = stream::unfold(connection.clone(), |connection| async move {
            Some((receive_from_peer(&connection).await, connection))
        })
        .boxed();
        self.peers.insert(peer, connection);
        self.peer_messages.insert(peer, messages);
    }

    fn disconnect(&mut self, peer: EndpointId) {
        self.peers.remove(&peer);
        self.peer_messages.remove(&peer);
        log::info!("disconnected from peer {}", peer.fmt_short());
    }

    async fn broadcast(&mut self, channel: Channel, message: Bytes) -> Result<()> {
        let Some(sender) = self.senders.get(&channel) else {
            return Ok(());
        };

        log::trace!("broadcasting {} bytes to {channel}", message.len());
        sender.broadcast(message).await?;
        Ok(())
    }

    fn reset(&mut self) {
        self.senders.clear();
        self.receivers.clear();
        self.peers.clear();
        self.peer_messages.clear();
        log::info!("left every topic and peer");
    }

    fn on_event(&mut self, channel: Channel, event: Option<Result<GossipEvent, ApiError>>) {
        match event {
            Some(Ok(GossipEvent::NeighborUp(id))) => {
                log::debug!("connected to {} in {channel}", id.fmt_short());
            }
            Some(Ok(GossipEvent::NeighborDown(id))) => {
                log::debug!("disconnected from {} in {channel}", id.fmt_short());
            }
            Some(Ok(GossipEvent::Received(message))) => {
                log::trace!(
                    "received {} bytes from {} in {channel}",
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

    fn on_peer_message(&mut self, peer: EndpointId, message: Result<Bytes>) {
        match message {
            Ok(message) => {
                log::trace!("received {} bytes from {}", message.len(), peer.fmt_short());
                let _ = self
                    .events
                    .send(Event::Received(Channel::Peer(peer), message));
            }
            Err(err) => self.exit(Channel::Peer(peer), format!("connection lost: {err:#}")),
        }
    }

    fn exit(&mut self, channel: Channel, reason: String) {
        log::error!("lost {channel}: {reason}");
        match channel {
            Channel::Topic(_) => self.unsubscribe(channel),
            Channel::Peer(peer) => self.disconnect(peer),
        }
        let _ = self.events.send(Event::Exit(channel));
    }
}

async fn receive_from_peer(connection: &Connection) -> Result<Bytes> {
    let mut recv = connection.accept_uni().await?;
    Ok(recv.read_to_end(MAX_MESSAGE_SIZE).await?.into())
}

/// Hands the connections that peers open to us to the actor.
#[derive(Debug, Clone)]
struct PeerProtocol {
    connections: UnboundedSender<Connection>,
}

impl ProtocolHandler for PeerProtocol {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        // The actor keeps the connection open after we return.
        let _ = self.connections.send(connection);
        Ok(())
    }
}
