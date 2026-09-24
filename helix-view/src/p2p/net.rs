//! The transport layer moves opaque bytes between peers.

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
    Received(Bytes),
    Quit(String),
}

/// What the editor asks to the service.
#[derive(Debug)]
enum Request {
    Ticket(oneshot::Sender<String>),
    Join(String, oneshot::Sender<Result<()>>),
    Leave,
    Broadcast(Bytes),
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

    /// Sends to every member of the session. Delivery is best-effort.
    pub fn broadcast(&self, message: Bytes) {
        self.send(Request::Broadcast(message));
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

/// The Service's internal actor
struct Actor {
    endpoint: Endpoint,
    _router: Router,
    gossip: Gossip,
    /// Addresses learned from tickets.
    addresses: MemoryLookup,
    events: UnboundedSender<Event>,
    /// The swarm we are in, if any.
    topic: Option<(TopicId, GossipTopic)>,
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
            Request::Broadcast(message) => {
                if let Err(err) = self.broadcast(message).await {
                    log::warn!("failed to broadcast: {err:#}");
                }
            }
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
        ensure!(self.topic.is_none(), "already in a session");

        let bootstrap = addr.id;
        self.addresses.add_endpoint_info(addr);
        log::info!(
            "joining session {} through {}",
            topic.fmt_short(),
            bootstrap.fmt_short()
        );
        let subscription = self.gossip.subscribe(topic, vec![bootstrap]).await?;
        self.topic = Some((topic, subscription));
        Ok(())
    }

    async fn broadcast(&mut self, message: Bytes) -> Result<()> {
        let Some((_, subscription)) = &mut self.topic else {
            return Ok(());
        };

        log::trace!("broadcasting {} bytes", message.len());
        subscription.broadcast(message).await?;
        Ok(())
    }

    fn leave(&mut self) {
        if let Some((topic, _)) = self.topic.take() {
            log::info!("left session {}", topic.fmt_short());
        }
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
                log::trace!(
                    "received {} bytes from {}",
                    message.content.len(),
                    message.delivered_from.fmt_short()
                );
                let _ = self.events.send(Event::Received(message.content));
            }
            Some(Ok(GossipEvent::Lagged)) => self.quit("fell behind the session".into()),
            Some(Err(err)) => self.quit(format!("gossip failed: {err:#}")),
            None => self.quit("gossip stream ended".into()),
        }
    }

    fn quit(&mut self, reason: String) {
        self.topic.take();
        log::error!("quit session: {reason}");
        let _ = self.events.send(Event::Quit(reason));
    }
}
