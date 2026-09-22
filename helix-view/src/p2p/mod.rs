pub mod proto;

use std::collections::HashMap;

use anyhow::{ensure, Result};
use helix_core::crdt::SharedId;
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
use tokio_stream::{wrappers::UnboundedReceiverStream, StreamMap};

use crate::DocumentId;
use proto::{Announcement, FileMessage, SessionMessage, SessionTicket};

/// Gossip defaults to 4 KiB, and a Snapshot carries a whole buffer.
const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

#[derive(Debug)]
pub enum Event {
    /// A neighbour came up on the session topic.
    Connected(EndpointId),
    Announced(Announcement),
    /// A neighbour came up on a file's topic.
    FileConnected(SharedId),
    File(SharedId, FileMessage),
    /// We left a file's topic on our own, so the file is no longer shared.
    FileLeft(SharedId),
    /// We left the session on our own, so nothing is shared any more.
    Left,
    Error(String),
}

#[derive(Debug)]
pub enum Request {
    Ticket(Sender<String>),
    Join(String),
    Close,
    Announce(Announcement),
    Subscribe(Announcement),
    Unsubscribe(SharedId),
    Broadcast(SharedId, FileMessage),
}

/// Handle to the actor task that owns the Session.
///
/// It also keeps the editor's side of the session. That lives here and not
/// in the actor because the editor reads it on its own thread, like the
/// picker listing the files, and couldn't wait on the actor for it.
pub struct Service {
    pub id: EndpointId,
    pub events: UnboundedReceiverStream<Event>,
    pub requests: UnboundedSender<Request>,
    /// Every file announced in the current session, open or not.
    pub files: HashMap<SharedId, Announcement>,
    /// Files we subscribed to and are waiting on a snapshot of, with the
    /// empty buffer that will hold each.
    pub pending: HashMap<SharedId, DocumentId>,
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

            let gossip = Gossip::builder()
                .max_message_size(MAX_MESSAGE_SIZE)
                .spawn(endpoint.clone());
            let _router = Router::builder(endpoint.clone())
                .accept(ALPN, gossip.clone())
                .spawn();

            let mut session = Session::new(endpoint, gossip, events_tx);

            loop {
                tokio::select! {
                    request = requests_rx.recv() => {
                        let Some(request) = request else {
                            break;
                        };
                        session.handle(request).await;
                    }
                    incoming = session.next_event() => session.on_event(incoming),
                }
            }
        });

        Service {
            id,
            events: UnboundedReceiverStream::new(events_rx),
            requests: requests_tx,
            files: HashMap::new(),
            pending: HashMap::new(),
        }
    }
}

impl Default for Service {
    fn default() -> Self {
        Self::new()
    }
}

/// Something that happened on the session topic or on a file's topic.
enum Incoming {
    Session(Option<Result<GossipEvent, ApiError>>),
    File(SharedId, Result<GossipEvent, ApiError>),
}

/// A collaborative session, as a protocol on top of gossip.
///
/// The session topic announces which files are shared, and each file's
/// edits travel on a topic of its own, which only the peers that have the
/// file open subscribe to.
struct Session {
    endpoint: Endpoint,
    gossip: Gossip,
    /// Addresses learned from tickets for gossip to dial,
    /// as it dials bootstrap peers by id alone.
    addresses: MemoryLookup,
    events: UnboundedSender<Event>,
    /// The swarm we are in, if any.
    topic: Option<(TopicId, GossipTopic)>,
    /// The topics of the files we have open. A StreamMap so that the actor
    /// can wait on all of them at once, and learn which file an event is for.
    files: StreamMap<SharedId, GossipTopic>,
}

impl Session {
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
            files: StreamMap::new(),
        }
    }

    async fn handle(&mut self, request: Request) {
        let result = match request {
            Request::Ticket(chan) => {
                let _ = chan.send(self.ticket().await).await;
                Ok(())
            }
            Request::Join(ticket) => self.join(&ticket).await,
            Request::Close => {
                self.close();
                Ok(())
            }
            Request::Announce(announcement) => self.announce(announcement).await,
            Request::Subscribe(announcement) => self.subscribe(announcement).await,
            Request::Unsubscribe(id) => {
                self.unsubscribe(id);
                Ok(())
            }
            Request::Broadcast(id, message) => self.broadcast(id, message).await,
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

    async fn announce(&mut self, announcement: Announcement) -> Result<()> {
        let Some((_, subscription)) = &mut self.topic else {
            return Ok(());
        };

        let message = SessionMessage::Announce(announcement);
        subscription
            .broadcast(proto::encode(&message)?.into())
            .await?;
        Ok(())
    }

    async fn subscribe(&mut self, announcement: Announcement) -> Result<()> {
        if self.files.contains_key(&announcement.id) {
            return Ok(());
        }

        // Whoever has the file open is in its topic, so bootstrap from its
        // owner and from our neighbours in the session. Gossip drops joins
        // sent to peers that aren't in the topic, so guessing wrong is harmless.
        // When we are the owner, this may leave nobody to bootstrap from,
        // which is fine: the others come to us when they open the file.
        let mut bootstrap = vec![announcement.owner];
        if let Some((_, session)) = &self.topic {
            bootstrap.extend(session.neighbors());
        }
        let me = self.endpoint.id();
        bootstrap.retain(|peer| *peer != me);

        let topic = TopicId::from_bytes(*announcement.id.as_bytes());
        let subscription = self.gossip.subscribe(topic, bootstrap).await?;
        self.files.insert(announcement.id, subscription);
        Ok(())
    }

    /// Dropping a topic's subscription is what leaves it.
    fn unsubscribe(&mut self, id: SharedId) {
        self.files.remove(&id);
    }

    async fn broadcast(&mut self, id: SharedId, message: FileMessage) -> Result<()> {
        // A StreamMap can't look a topic up by key, but we only ever have
        // a handful of files open.
        let Some((_, subscription)) = self.files.iter_mut().find(|(file, _)| *file == id) else {
            // We may have left the file's topic after falling behind.
            return Ok(());
        };

        // Only best-effort delivery.
        subscription
            .broadcast(proto::encode(&message)?.into())
            .await?;
        Ok(())
    }

    fn close(&mut self) {
        self.topic = None;
        self.files.clear();
    }

    async fn next_event(&mut self) -> Incoming {
        // Borrow the fields apart, as both futures below borrow them mutably.
        let Self { topic, files, .. } = self;

        let session = async {
            match topic {
                Some((_, subscription)) => subscription.next().await,
                None => std::future::pending().await,
            }
        };

        tokio::select! {
            event = session => Incoming::Session(event),
            // An empty StreamMap yields None at once, which disables this
            // branch until the next loop.
            Some((id, event)) = files.next() => Incoming::File(id, event),
        }
    }

    fn on_event(&mut self, incoming: Incoming) {
        match incoming {
            Incoming::Session(event) => self.on_session_event(event),
            Incoming::File(id, event) => self.on_file_event(id, event),
        }
    }

    fn on_session_event(&mut self, event: Option<Result<GossipEvent, ApiError>>) {
        match event {
            Some(Ok(GossipEvent::NeighborUp(id))) => {
                log::info!("connected to {}", id.fmt_short());
                let _ = self.events.send(Event::Connected(id));
            }
            Some(Ok(GossipEvent::NeighborDown(id))) => {
                log::info!("disconnected from {}", id.fmt_short());
            }
            Some(Ok(GossipEvent::Received(message))) => {
                match proto::decode::<SessionMessage>(&message.content) {
                    Ok(SessionMessage::Announce(announcement)) => {
                        let _ = self.events.send(Event::Announced(announcement));
                    }
                    Err(err) => self.report(format!(
                        "bad message from {}: {:#}",
                        message.delivered_from.fmt_short(),
                        err
                    )),
                }
            }
            // We may have missed announcements, which is harmless: they are
            // sent again whenever someone connects. Edits, which can't be
            // recovered, travel on the file topics instead.
            Some(Ok(GossipEvent::Lagged)) => {
                log::warn!("fell behind the session");
            }
            Some(Err(err)) => self.leave(format!("session failed: {:#}", err)),
            None => self.leave("left the session".into()),
        }
    }

    // There is no case for a file's topic ending: a StreamMap drops it
    // without telling us, and it only happens once gossip itself stopped.
    fn on_file_event(&mut self, id: SharedId, event: Result<GossipEvent, ApiError>) {
        match event {
            // Someone joined the file's topic, which means they opened it and
            // are waiting for its contents.
            Ok(GossipEvent::NeighborUp(peer)) => {
                log::info!("{} opened {}", peer.fmt_short(), id.fmt_short());
                let _ = self.events.send(Event::FileConnected(id));
            }
            Ok(GossipEvent::NeighborDown(peer)) => {
                log::info!("{} closed {}", peer.fmt_short(), id.fmt_short());
            }
            Ok(GossipEvent::Received(message)) => {
                match proto::decode::<FileMessage>(&message.content) {
                    Ok(file_message) => {
                        let _ = self.events.send(Event::File(id, file_message));
                    }
                    Err(err) => self.report(format!(
                        "bad message from {}: {:#}",
                        message.delivered_from.fmt_short(),
                        err
                    )),
                }
            }
            // We lost some edits, which are lost for good, so stop sharing
            // the file instead of drifting apart unnoticed. Only this file is
            // affected, the rest of the session goes on.
            Ok(GossipEvent::Lagged) => {
                self.leave_file(id, format!("fell behind on {} and left it", id.fmt_short()));
            }
            Err(err) => self.leave_file(id, format!("{} failed: {:#}", id.fmt_short(), err)),
        }
    }

    /// Leave the session on our own, and tell the editor.
    fn leave(&mut self, error: String) {
        self.report(error);
        self.close();
        let _ = self.events.send(Event::Left);
    }

    /// Leave a file's topic on our own, and tell the editor.
    fn leave_file(&mut self, id: SharedId, error: String) {
        self.report(error);
        self.unsubscribe(id);
        let _ = self.events.send(Event::FileLeft(id));
    }

    fn report(&self, error: String) {
        log::error!("{error}");
        let _ = self.events.send(Event::Error(error));
    }
}
