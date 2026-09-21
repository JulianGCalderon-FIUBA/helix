pub mod proto;

use anyhow::{bail, ensure, Result};
use iroh::{
    address_lookup::memory::MemoryLookup, endpoint::presets, protocol::Router, Endpoint,
    EndpointId, SecretKey,
};
use iroh_gossip::{
    api::{ApiError, Event as GossipEvent, GossipTopic},
    Gossip, TopicId, ALPN,
};
use iroh_tickets::Ticket;
use n0_future::StreamExt;
use tokio::sync::mpsc::{unbounded_channel, Sender, UnboundedSender};
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

/// Handle to the actor task that owns the Session.
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

            // Gossip opens and keeps its own connections to the members of each
            // topic, and relays broadcasts through them. The router hands it the
            // incoming connections that ask for its ALPN.
            let gossip = Gossip::builder()
                .max_message_size(MAX_MESSAGE_SIZE)
                .spawn(endpoint.clone());
            let _router = Router::builder(endpoint.clone())
                .accept(ALPN, gossip.clone())
                .spawn();

            let mut session = Session::new(endpoint, gossip, events_tx);

            // One task serves both the editor's requests and gossip's events,
            // so the session needs no locks.
            loop {
                tokio::select! {
                    request = requests_rx.recv() => {
                        let Some(request) = request else {
                            break;
                        };
                        session.handle(request).await;
                    }
                    event = session.next_event() => session.on_event(event),
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

/// A collaborative session, as a protocol on top of gossip.
struct Session {
    endpoint: Endpoint,
    gossip: Gossip,
    /// Addresses learned from tickets, for gossip to dial.
    addresses: MemoryLookup,
    events: UnboundedSender<Event>,
    /// The swarm we are in, if any. A topic is a gossip swarm: everyone
    /// subscribed to the same id receives everyone's broadcasts. The
    /// subscription is both how we broadcast and a stream of what happens
    /// in the swarm, and dropping it leaves.
    topic: Option<(TopicId, GossipTopic)>,
}

impl Session {
    fn new(endpoint: Endpoint, gossip: Gossip, events: UnboundedSender<Event>) -> Self {
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
            topic: None,
        }
    }

    async fn handle(&mut self, request: Request) {
        let result = match request {
            Request::Ticket(chan) => {
                let _ = chan.send(self.ticket().await).await;
                Ok(())
            }
            Request::Join(ticket) => self.join(&ticket).await,
            Request::Peers(chan) => {
                let _ = chan.send(self.peers()).await;
                Ok(())
            }
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

    /// Invites into the current session, starting one if there is none.
    async fn ticket(&mut self) -> String {
        let topic = match &self.topic {
            Some((topic, _)) => *topic,
            None => {
                // Gossip lets in anyone who knows the topic id, so a random id
                // makes the ticket the only way in.
                let topic = TopicId::from_bytes(rand::random());
                // With no one to bootstrap from, this starts an empty swarm
                // that others join through us.
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
        if let Some((current, _)) = &self.topic {
            ensure!(*current != topic, "already in this session");
            bail!("already in a session, close it before joining another");
        }

        // We only need one member to get in. Gossip introduces us to the
        // rest of the swarm by itself.
        let bootstrap = addr.id;
        self.addresses.add_endpoint_info(addr);
        let subscription = self.gossip.subscribe(topic, vec![bootstrap]).await?;
        self.topic = Some((topic, subscription));
        Ok(())
    }

    async fn broadcast(&mut self, message: Message) -> Result<()> {
        let Some((_, subscription)) = &mut self.topic else {
            return Ok(());
        };

        // Reaches every member, relayed hop by hop if needed, but best-effort:
        // a message can be lost or overtaken by a later one.
        subscription
            .broadcast(proto::encode(&message)?.into())
            .await?;
        Ok(())
    }

    /// Gossip only knows its direct neighbours, not every member. It keeps
    /// up to five, so in a session of a few peers that is everyone.
    fn peers(&self) -> Vec<EndpointId> {
        self.topic
            .as_ref()
            .map(|(_, subscription)| subscription.neighbors().collect())
            .unwrap_or_default()
    }

    /// Leaves the topic: gossip does so once the subscription is dropped.
    fn close(&mut self) {
        let Some((_, subscription)) = self.topic.take() else {
            return;
        };

        for id in subscription.neighbors() {
            log::info!("disconnected from {}", id.fmt_short());
            let _ = self.events.send(Event::Disconnected(id));
        }
    }

    async fn next_event(&mut self) -> Option<Result<GossipEvent, ApiError>> {
        match &mut self.topic {
            Some((_, subscription)) => subscription.next().await,
            // Outside a session there is nothing to wait for, so the loop
            // only serves requests.
            None => std::future::pending().await,
        }
    }

    fn on_event(&mut self, event: Option<Result<GossipEvent, ApiError>>) {
        match event {
            // A direct connection to a member came up. It may be a newcomer,
            // which is why the editor offers it every shared buffer.
            Some(Ok(GossipEvent::NeighborUp(id))) => {
                log::info!("connected to {}", id.fmt_short());
                let _ = self.events.send(Event::Connected(id));
            }
            Some(Ok(GossipEvent::NeighborDown(id))) => {
                log::info!("disconnected from {}", id.fmt_short());
                let _ = self.events.send(Event::Disconnected(id));
            }
            // A broadcast from some member. `delivered_from` is the neighbour
            // that relayed it, not necessarily who wrote it.
            Some(Ok(GossipEvent::Received(message))) => match proto::decode(&message.content) {
                Ok(message) => {
                    let _ = self.events.send(Event::Message(message));
                }
                Err(err) => self.report(format!(
                    "bad message from {}: {:#}",
                    message.delivered_from.fmt_short(),
                    err
                )),
            },
            // We read events slower than they arrived, so gossip dropped some.
            // The subscription carries on, but what it dropped is lost.
            Some(Ok(GossipEvent::Lagged)) => {
                self.report("fell behind the session, some edits were lost".into());
            }
            // The subscription is gone, so we are out of the swarm.
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
