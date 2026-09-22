use std::future::Future;

use anyhow::{ensure, Result};
use bytes::Bytes;
use iroh::{
    address_lookup::memory::MemoryLookup, endpoint::presets, protocol::Router, Endpoint,
    EndpointAddr, EndpointId, SecretKey,
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
const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

#[derive(Debug)]
pub enum Event {
    NeighborUp(EndpointId),
    Received(Bytes),
    Error(String),
}

#[derive(Debug)]
enum Request {
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

        let secret_key = SecretKey::generate();
        let id = secret_key.public();

        tokio::spawn(async move {
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
            let _router = Router::builder(endpoint.clone())
                .accept(ALPN, gossip.clone())
                .spawn();

            let mut node = Node::new(endpoint, gossip, events_tx);

            loop {
                tokio::select! {
                    request = requests_rx.recv() => {
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
    /// The future does not borrow the service, so it can run as a job.
    pub fn ticket(&self) -> impl Future<Output = String> + 'static {
        let (tx, rx) = oneshot::channel();
        self.send(Request::Ticket(tx));
        async move { rx.await.expect("node should reply with a ticket") }
    }

    /// Failures come back as an [`Event::Error`].
    pub fn join(&self, ticket: String) {
        self.send(Request::Join(ticket));
    }

    pub fn close(&self) {
        self.send(Request::Close);
    }

    pub fn broadcast(&self, message: Bytes) {
        self.send(Request::Broadcast(message));
    }

    fn send(&self, request: Request) {
        self.requests
            .send(request)
            .expect("p2p node should be running");
    }
}

/// An invitation into a session: its topic and one member to dial.
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

/// Our endpoint in the session's gossip swarm.
struct Node {
    endpoint: Endpoint,
    gossip: Gossip,
    /// Addresses learned from tickets for gossip to dial,
    /// as it dials bootstrap peers by id alone.
    addresses: MemoryLookup,
    events: UnboundedSender<Event>,
    /// The swarm we are in, if any.
    topic: Option<(TopicId, GossipTopic)>,
}

impl Node {
    fn new(endpoint: Endpoint, gossip: Gossip, events: UnboundedSender<Event>) -> Self {
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

        // Only best-effort delivery.
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
            // leave instead of drifting apart unnoticed.
            Some(Ok(GossipEvent::Lagged)) => {
                self.report("fell behind the session and left it".into());
                self.close();
            }
            Some(Err(err)) => {
                self.report(format!("session failed: {:#}", err));
                self.close();
            }
            None => {
                self.report("left the session".into());
                self.close();
            }
        }
    }

    fn report(&self, error: String) {
        log::error!("{error}");
        let _ = self.events.send(Event::Error(error));
    }
}
