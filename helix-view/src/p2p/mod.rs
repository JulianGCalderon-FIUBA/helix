pub mod proto;

use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
};

use anyhow::{bail, ensure, Result};
use iroh::{
    address_lookup::memory::MemoryLookup, endpoint::presets, protocol::Router, Endpoint,
    EndpointId, SecretKey,
};
use iroh_gossip::{
    api::{Event as GossipEvent, GossipReceiver, GossipSender},
    Gossip, TopicId, ALPN,
};
use iroh_tickets::Ticket;
use n0_future::StreamExt;
use tokio::{
    sync::mpsc::{unbounded_channel, Sender, UnboundedSender},
    task::JoinHandle,
};
use tokio_stream::wrappers::UnboundedReceiverStream;

use proto::{Message, SessionTicket};

/// Gossip defaults to 4 KiB, and a Share carries a whole buffer.
const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

#[derive(Debug)]
pub enum Event {
    Connected(EndpointId),
    Disconnected(EndpointId),
    Message(Message),
    Error(String),
}

#[derive(Debug)]
pub enum Request {
    Ticket(Sender<String>),
    Join(String),
    Peers(Sender<Vec<EndpointId>>),
    Close,
    Broadcast(Message),
}

/// Handle to the actor task that owns the Node.
pub struct Service {
    pub id: EndpointId,
    pub events: UnboundedReceiverStream<Event>,
    pub requests: UnboundedSender<Request>,
}

impl Service {
    pub fn new() -> Self {
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

            let mut node = Node::new(endpoint, events_tx);

            while let Some(request) = requests_rx.recv().await {
                let result = match request {
                    Request::Ticket(chan) => match node.ticket().await {
                        Ok(ticket) => {
                            let _ = chan.send(ticket).await;
                            continue;
                        }
                        Err(err) => Err(err),
                    },
                    Request::Join(ticket) => node.join(&ticket).await,
                    Request::Peers(chan) => {
                        let _ = chan.send(node.peers()).await;
                        continue;
                    }
                    Request::Close => {
                        node.close();
                        continue;
                    }
                    Request::Broadcast(message) => node.broadcast(message).await,
                };

                if let Err(err) = result {
                    report(&node.events, format!("{:#}", err));
                }
            }
        });

        Service {
            id,
            events: UnboundedReceiverStream::new(events_rx),
            requests: requests_tx,
        }
    }
}

impl Default for Service {
    fn default() -> Self {
        Self::new()
    }
}

/// Membership of the gossip topic for the session we are in.
struct Session {
    topic: TopicId,
    // Swapped out by the listener when it has to resubscribe.
    sender: Arc<Mutex<GossipSender>>,
    neighbors: Arc<Mutex<HashSet<EndpointId>>>,
    listener: JoinHandle<()>,
}

/// The local node. Gossip owns the connections; the node only tracks the session.
struct Node {
    endpoint: Endpoint,
    gossip: Gossip,
    addresses: MemoryLookup,
    events: UnboundedSender<Event>,
    session: Option<Session>,
    _router: Router,
}

impl Node {
    fn new(endpoint: Endpoint, events: UnboundedSender<Event>) -> Self {
        let gossip = Gossip::builder()
            .max_message_size(MAX_MESSAGE_SIZE)
            .spawn(endpoint.clone());
        let router = Router::builder(endpoint.clone())
            .accept(ALPN, gossip.clone())
            .spawn();

        // Gossip dials bootstrap peers by id alone, so the address from a
        // ticket has to be findable through the endpoint.
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
            session: None,
            _router: router,
        }
    }

    /// Invites into the current session, starting one if there is none.
    async fn ticket(&mut self) -> Result<String> {
        let topic = match &self.session {
            Some(session) => session.topic,
            None => {
                let topic = TopicId::from_bytes(rand::random());
                self.subscribe(topic, Vec::new()).await?;
                topic
            }
        };

        Ok(SessionTicket {
            topic,
            addr: self.endpoint.addr(),
        }
        .encode_string())
    }

    async fn join(&mut self, ticket: &str) -> Result<()> {
        let SessionTicket { topic, addr } = SessionTicket::decode_string(ticket)?;

        ensure!(
            addr.id != self.endpoint.id(),
            "cannot join your own session"
        );
        if let Some(session) = &self.session {
            ensure!(session.topic != topic, "already in this session");
            bail!("already in a session, close it before joining another");
        }

        let bootstrap = addr.id;
        self.addresses.add_endpoint_info(addr);
        self.subscribe(topic, vec![bootstrap]).await
    }

    async fn subscribe(&mut self, topic: TopicId, bootstrap: Vec<EndpointId>) -> Result<()> {
        let (sender, receiver) = self.gossip.subscribe(topic, bootstrap).await?.split();
        let sender = Arc::new(Mutex::new(sender));
        let neighbors = Arc::default();

        let listener = tokio::spawn(listen(
            self.gossip.clone(),
            topic,
            receiver,
            Arc::clone(&sender),
            Arc::clone(&neighbors),
            self.events.clone(),
        ));

        self.session = Some(Session {
            topic,
            sender,
            neighbors,
            listener,
        });
        Ok(())
    }

    async fn broadcast(&self, message: Message) -> Result<()> {
        let Some(session) = &self.session else {
            return Ok(());
        };

        let body = proto::encode(&message)?;
        let sender = session.sender.lock().unwrap().clone();
        sender.broadcast(body.into()).await?;
        Ok(())
    }

    fn peers(&self) -> Vec<EndpointId> {
        self.session
            .as_ref()
            .map(|session| session.neighbors.lock().unwrap().iter().copied().collect())
            .unwrap_or_default()
    }

    /// Leaves the topic: gossip does so once both halves of the subscription are dropped.
    fn close(&mut self) {
        let Some(session) = self.session.take() else {
            return;
        };

        session.listener.abort();
        for id in session.neighbors.lock().unwrap().drain() {
            log::info!("disconnected from {}", id.fmt_short());
            let _ = self.events.send(Event::Disconnected(id));
        }
    }
}

async fn listen(
    gossip: Gossip,
    topic: TopicId,
    mut receiver: GossipReceiver,
    sender: Arc<Mutex<GossipSender>>,
    neighbors: Arc<Mutex<HashSet<EndpointId>>>,
    events: UnboundedSender<Event>,
) {
    loop {
        let event = match receiver.next().await {
            Some(Ok(event)) => event,
            Some(Err(err)) => {
                report(&events, format!("session failed: {:#}", err));
                break;
            }
            None => break,
        };

        match event {
            GossipEvent::NeighborUp(id) => {
                if neighbors.lock().unwrap().insert(id) {
                    log::info!("connected to {}", id.fmt_short());
                    let _ = events.send(Event::Connected(id));
                }
            }
            GossipEvent::NeighborDown(id) => {
                if neighbors.lock().unwrap().remove(&id) {
                    log::info!("disconnected from {}", id.fmt_short());
                    let _ = events.send(Event::Disconnected(id));
                }
            }
            GossipEvent::Received(message) => match proto::decode(&message.content) {
                Ok(message) => {
                    let _ = events.send(Event::Message(message));
                }
                Err(err) => report(
                    &events,
                    format!(
                        "bad message from {}: {:#}",
                        message.delivered_from.fmt_short(),
                        err
                    ),
                ),
            },
            GossipEvent::Lagged => {
                // Gossip closes a subscription that falls behind, so rejoin
                // through the neighbours we still know about. Whatever was
                // dropped in between is lost.
                report(
                    &events,
                    "fell behind the session, some edits were lost".into(),
                );

                let bootstrap = neighbors.lock().unwrap().iter().copied().collect();
                match gossip.subscribe(topic, bootstrap).await {
                    Ok(subscription) => {
                        let (new_sender, new_receiver) = subscription.split();
                        *sender.lock().unwrap() = new_sender;
                        receiver = new_receiver;
                    }
                    Err(err) => {
                        report(&events, format!("failed to rejoin the session: {:#}", err));
                        break;
                    }
                }
            }
        }
    }

    for id in neighbors.lock().unwrap().drain() {
        let _ = events.send(Event::Disconnected(id));
    }
}

fn report(events: &UnboundedSender<Event>, error: String) {
    log::error!("{error}");
    let _ = events.send(Event::Error(error));
}
