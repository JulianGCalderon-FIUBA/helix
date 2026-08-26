//! Membership of a collaboration session.
//!
//! A session is a full mesh: every peer holds one connection to every other
//! peer. A peer joins by dialing any member with a ticket, and is answered
//! with the addresses of the rest of the session.
//!
//! The mesh repairs itself. Peers keep telling each other who they are
//! connected to, and a peer dials whoever it is missing, so a hole left by two
//! peers joining at the same moment, or by a link that broke, closes on its
//! own. Every dial is idempotent, which is what lets the same peer be announced
//! any number of times.

use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{anyhow, bail, ensure, Context, Result};
use iroh::{
    endpoint::{Connection, ConnectionError, RecvStream, SendStream},
    protocol::{AcceptError, ProtocolHandler},
    Endpoint, EndpointAddr, EndpointId,
};
use iroh_tickets::endpoint::EndpointTicket;
use rand::Rng;
use tokio::{
    sync::mpsc::{channel, error::TrySendError, Receiver, Sender, UnboundedSender},
    time::{sleep, timeout},
};

use super::{
    proto::{self, Message},
    Event, ALPN,
};

/// Why a connection was closed. A peer that hears `BYE` is gone and is not
/// dialed again; one that hears `DUPLICATE` or `OVERRUN` is still there, and it
/// is the connection that went.
const BYE: u32 = 0;
const DUPLICATE: u32 = 1;
const OVERRUN: u32 = 2;

const DIAL_TIMEOUT: Duration = Duration::from_secs(30);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// How often a peer tells the session who it is connected to.
const GOSSIP_INTERVAL: Duration = Duration::from_secs(15);

/// How many messages may be waiting for one peer. The layer above cannot use a
/// stream that quietly skipped a message, so a peer this far behind loses its
/// connection instead, and the redial resynchronises it.
const OUTBOX_CAPACITY: usize = 1024;

const REDIAL_DELAY: Duration = Duration::from_millis(500);
const REDIAL_DELAY_MAX: Duration = Duration::from_secs(30);
const REDIAL_JITTER: Duration = Duration::from_millis(250);
/// How many times in a row a dial may fail before the peer is given up on.
const REDIAL_ATTEMPTS: u32 = 20;

#[derive(Debug)]
struct Peer {
    addr: EndpointAddr,
    /// Names the connection rather than the peer, so that the task serving one
    /// can tell "the peer left" from "the peer is here, on the connection that
    /// replaced mine".
    token: u64,
    /// Whether this is the connection of the pair that both sides keep.
    canonical: bool,
    connection: Connection,
    outbox: Sender<Message>,
}

/// Everything one lock covers, because the decisions taken here span all of it:
/// admitting a connection reads the peer it would replace, the session it
/// belongs to, and the dial it completes.
#[derive(Debug, Default)]
struct State {
    /// Bumped whenever the user closes the session. Work that was in flight
    /// carries the generation it started in and is turned away, so a dial that
    /// lands late cannot reopen a session that was closed.
    generation: u64,
    /// Handed out to connections, never reused.
    tokens: u64,
    peers: HashMap<EndpointId, Peer>,
    /// Peers a task is trying to reach.
    dialing: HashSet<EndpointId>,
}

#[derive(Debug, Clone)]
pub struct Session {
    endpoint: Endpoint,
    events: UnboundedSender<Event>,
    state: Arc<Mutex<State>>,
}

/// What a connection got out of being admitted.
struct Admission {
    token: u64,
    queue: Receiver<Message>,
    /// The rest of the session, taken while the peer was let in.
    peers: Vec<EndpointAddr>,
    /// False when this connection replaced another one to the same peer, which
    /// the editor was already told about.
    fresh: bool,
}

/// How serving a connection ended, for the task that dialed it.
enum Outcome {
    /// Nothing left to do: the peer left, the session closed, or another
    /// connection to the same peer took over.
    Done,
    /// The connection broke while the peer was still wanted.
    Dropped,
}

/// Holds a peer's place while a task is reaching it, so that a welcome naming
/// the same peer twice, gossip repeating what we are already doing, and a
/// redial waiting out its backoff do not each open a connection.
struct Dialing {
    session: Session,
    id: EndpointId,
}

impl Drop for Dialing {
    fn drop(&mut self) {
        self.session.state.lock().unwrap().dialing.remove(&self.id);
    }
}

impl Session {
    pub fn new(endpoint: Endpoint, events: UnboundedSender<Event>) -> Self {
        let session = Self {
            endpoint,
            events,
            state: Arc::default(),
        };

        let gossiping = session.clone();
        tokio::spawn(async move {
            loop {
                sleep(GOSSIP_INTERVAL).await;
                gossiping.gossip();
            }
        });

        session
    }

    fn id(&self) -> EndpointId {
        self.endpoint.id()
    }

    pub fn emit(&self, event: Event) {
        let _ = self.events.send(event);
    }

    pub fn report(&self, error: String) {
        self.emit(Event::Error(error));
    }

    /// A ticket is an address: the session it names is whatever the peer at
    /// that address is part of when it is used.
    pub fn ticket_new(&self) -> String {
        EndpointTicket::new(self.endpoint.addr()).to_string()
    }

    pub fn ticket_join(&self, ticket: &str) -> Result<()> {
        let ticket = EndpointTicket::from_str(ticket)?;
        self.dial(ticket.endpoint_addr().clone())
    }

    pub fn ticket_peers(&self) -> Vec<EndpointId> {
        self.state.lock().unwrap().peers.keys().copied().collect()
    }

    pub fn ticket_close(&self) -> Result<()> {
        let peers = {
            let mut state = self.state.lock().unwrap();
            state.generation += 1;
            state.dialing.clear();
            std::mem::take(&mut state.peers)
        };

        for (id, peer) in peers {
            peer.connection.close(BYE.into(), b"bye");
            self.emit(Event::Disconnected(id));
        }

        Ok(())
    }

    pub fn broadcast(&self, data: Vec<u8>) -> Result<()> {
        let state = self.state.lock().unwrap();
        ensure!(!state.peers.is_empty(), "not in a session");

        for (id, peer) in state.peers.iter() {
            self.post(*id, peer, Message::Data(data.clone()));
        }

        Ok(())
    }

    /// Reaches a peer, retrying while the connection keeps breaking. Turns away
    /// a peer that is already connected or already being reached, which is what
    /// makes it safe to call for every address a welcome or gossip carries.
    fn dial(&self, addr: EndpointAddr) -> Result<()> {
        let id = addr.id;
        ensure!(id != self.id(), "cannot join your own session");

        let generation = {
            let mut state = self.state.lock().unwrap();
            ensure!(
                !state.peers.contains_key(&id),
                "already connected to {}",
                id.fmt_short()
            );
            ensure!(
                state.dialing.insert(id),
                "already connecting to {}",
                id.fmt_short()
            );
            state.generation
        };

        let dialing = Dialing {
            session: self.clone(),
            id,
        };

        let session = self.clone();
        tokio::spawn(async move { session.redial(generation, addr, dialing).await });

        Ok(())
    }

    /// Keeps a peer connected. The place in `dialing` is held for as long as
    /// this runs, waits included, so nothing else dials the peer behind it.
    async fn redial(&self, generation: u64, addr: EndpointAddr, _dialing: Dialing) {
        let id = addr.id;
        let mut delay = REDIAL_DELAY;
        let mut failures = 0;

        loop {
            match self.connect(generation, addr.clone()).await {
                Ok(Outcome::Done) => return,
                Ok(Outcome::Dropped) => {
                    // The peer was reachable a moment ago, so start over rather
                    // than carry a backoff from whenever it was last down.
                    delay = REDIAL_DELAY;
                    failures = 0;
                }
                Err(err) => {
                    // Only the first failure is worth a message. The ones after
                    // it are the same failure, once per attempt.
                    if failures == 0 {
                        self.report(format!(
                            "failed to connect to {}: {:#}",
                            id.fmt_short(),
                            err
                        ));
                    }

                    failures += 1;
                    if failures >= REDIAL_ATTEMPTS {
                        self.report(format!("gave up reaching {}", id.fmt_short()));
                        return;
                    }
                }
            }

            // Jittered, so that a session that lost a peer all at once does not
            // dial it back in lockstep.
            let jitter = rand::rng().random_range(Duration::ZERO..REDIAL_JITTER);
            sleep(delay + jitter).await;
            delay = (delay * 2).min(REDIAL_DELAY_MAX);

            if !self.wanted(generation, id) {
                return;
            }
        }
    }

    async fn connect(&self, generation: u64, addr: EndpointAddr) -> Result<Outcome> {
        let connection = timeout(DIAL_TIMEOUT, self.endpoint.connect(addr.clone(), ALPN))
            .await
            .context("timed out dialing")??;

        let (mut send, mut recv) = timeout(HANDSHAKE_TIMEOUT, connection.open_bi())
            .await
            .context("timed out opening the session stream")??;

        let hello = Message::Hello {
            addr: self.endpoint.addr(),
        };
        timeout(HANDSHAKE_TIMEOUT, proto::write(&mut send, &hello))
            .await
            .context("timed out sending the hello")??;

        let welcome = timeout(HANDSHAKE_TIMEOUT, proto::read(&mut recv))
            .await
            .context("timed out waiting for the welcome")??;
        let Some(Message::Welcome { peers }) = welcome else {
            bail!("expected a welcome");
        };

        let Some(admission) = self.enter(generation, addr.clone(), &connection, true) else {
            return Ok(Outcome::Done);
        };

        // The welcome names the rest of the session, which is what makes
        // joining transitive.
        self.meet(peers);

        Ok(self
            .serve(addr, connection, send, recv, admission, true)
            .await)
    }

    async fn answer(&self, generation: u64, connection: Connection) -> Result<()> {
        let (mut send, mut recv) = timeout(HANDSHAKE_TIMEOUT, connection.accept_bi())
            .await
            .context("timed out waiting for the session stream")??;

        let hello = timeout(HANDSHAKE_TIMEOUT, proto::read(&mut recv))
            .await
            .context("timed out waiting for the hello")??;
        let Some(Message::Hello { addr }) = hello else {
            bail!("expected a hello");
        };
        // The address is peer input, and the rest of the session will dial what
        // it says, so it has to describe the peer the connection authenticated.
        ensure!(
            addr.id == connection.remote_id(),
            "hello address does not match the connection identity",
        );

        let Some(admission) = self.enter(generation, addr.clone(), &connection, false) else {
            return Ok(());
        };

        // Written before the writer starts, so that it is the first frame on
        // the stream, and from a list taken as the newcomer was let in, so that
        // two peers admitted at the same moment are named to each other.
        let welcome = Message::Welcome {
            peers: admission.peers.clone(),
        };
        let written = match timeout(HANDSHAKE_TIMEOUT, proto::write(&mut send, &welcome)).await {
            Ok(result) => result,
            Err(_) => Err(anyhow!("timed out sending the welcome")),
        };

        if let Err(err) = written {
            if self.leave(addr.id, admission.token) {
                connection.close(BYE.into(), b"bye");
                if !admission.fresh {
                    // We took a peer the editor knows about off its old
                    // connection and then lost this one.
                    self.emit(Event::Disconnected(addr.id));
                }
            }
            return Err(err);
        }

        self.serve(addr, connection, send, recv, admission, false)
            .await;

        Ok(())
    }

    /// Files a connection under its peer, deciding between it and one that is
    /// already there.
    ///
    /// Two peers can dial each other at the same time and end up holding two
    /// connections. The pair keeps the one dialed by the lower endpoint id:
    /// both sides reach that from the connection alone, so neither has to ask
    /// the other which one to drop. A lone connection is kept whichever side
    /// opened it, since there is no pair to agree on.
    fn enter(
        &self,
        generation: u64,
        addr: EndpointAddr,
        connection: &Connection,
        dialed: bool,
    ) -> Option<Admission> {
        let id = addr.id;
        let canonical = dialed == (self.id() < id);

        let mut state = self.state.lock().unwrap();

        // The session this connection was opened for has been closed since.
        if state.generation != generation {
            connection.close(BYE.into(), b"bye");
            return None;
        }

        let mut fresh = true;
        if let Some(peer) = state.peers.get(&id) {
            // A connection that is already closed is not a tie to break: it is
            // the one that went, and this is what the peer has left.
            let dead = peer.connection.close_reason().is_some();
            let wins = dead || (canonical && !peer.canonical);
            if !wins {
                connection.close(DUPLICATE.into(), b"duplicate");
                return None;
            }

            peer.connection.close(DUPLICATE.into(), b"duplicate");
            // The peer never left, so the editor is not told that it did.
            fresh = false;
        }

        state.tokens += 1;
        let token = state.tokens;
        let (outbox, queue) = channel(OUTBOX_CAPACITY);

        state.peers.insert(
            id,
            Peer {
                addr,
                token,
                canonical,
                connection: connection.clone(),
                outbox,
            },
        );

        let peers = state
            .peers
            .values()
            .filter(|peer| peer.addr.id != id)
            .map(|peer| peer.addr.clone())
            .collect();

        Some(Admission {
            token,
            queue,
            peers,
            fresh,
        })
    }

    /// Takes a connection's peer out, unless the entry has since been filed by
    /// a newer connection to the same peer.
    fn leave(&self, id: EndpointId, token: u64) -> bool {
        let mut state = self.state.lock().unwrap();

        let ours = state.peers.get(&id).is_some_and(|peer| peer.token == token);
        if ours {
            state.peers.remove(&id);
        }

        ours
    }

    fn member(&self, id: EndpointId, token: u64) -> bool {
        let state = self.state.lock().unwrap();
        state.peers.get(&id).is_some_and(|peer| peer.token == token)
    }

    fn wanted(&self, generation: u64, id: EndpointId) -> bool {
        let state = self.state.lock().unwrap();
        state.generation == generation && !state.peers.contains_key(&id)
    }

    async fn serve(
        &self,
        addr: EndpointAddr,
        connection: Connection,
        send: SendStream,
        mut recv: RecvStream,
        admission: Admission,
        dialed: bool,
    ) -> Outcome {
        let id = addr.id;
        let token = admission.token;

        self.start_writer(connection.clone(), send, admission.queue);
        if admission.fresh {
            self.emit(Event::Connected(id));
        }
        // Tell the session about the peer that just arrived, so that a mesh
        // with a hole in it closes now rather than at the next round of gossip.
        self.gossip();

        let reconnect = loop {
            match proto::read(&mut recv).await {
                Ok(Some(message)) => self.handle(id, token, message),
                // A finished stream is how a peer says goodbye.
                Ok(None) => break false,
                Err(err) => {
                    let reason = connection.close_reason();
                    if reason.is_none() {
                        self.report(format!("failed to read from {}: {:#}", id.fmt_short(), err));
                    }
                    break reconnectable(reason);
                }
            }
        };

        // A connection that was replaced leaves the peer where it is: the
        // editor is hearing from it over the connection that took over.
        if !self.leave(id, token) {
            return Outcome::Done;
        }

        connection.close(BYE.into(), b"bye");
        self.emit(Event::Disconnected(id));

        if !reconnect {
            return Outcome::Done;
        }

        if dialed {
            // The task that dialed this connection is the one that retries it.
            Outcome::Dropped
        } else {
            // Nothing is retrying a connection we only answered, so start.
            let _ = self.dial(addr);
            Outcome::Done
        }
    }

    fn start_writer(
        &self,
        connection: Connection,
        mut send: SendStream,
        mut queue: Receiver<Message>,
    ) {
        let session = self.clone();
        tokio::spawn(async move {
            let id = connection.remote_id();

            while let Some(message) = queue.recv().await {
                if let Err(err) = proto::write(&mut send, &message).await {
                    if connection.close_reason().is_none() {
                        session.report(format!("failed to write to {}: {:#}", id.fmt_short(), err));
                    }
                    break;
                }
            }

            let _ = send.finish();
        });
    }

    /// Queues a message for a peer. One that cannot keep up loses its
    /// connection rather than its place in the order the sender wrote in.
    fn post(&self, id: EndpointId, peer: &Peer, message: Message) {
        match peer.outbox.try_send(message) {
            Ok(()) => {}
            // The writer is gone, which the task serving the peer is already
            // dealing with.
            Err(TrySendError::Closed(_)) => {}
            Err(TrySendError::Full(_)) => {
                peer.connection.close(OVERRUN.into(), b"too far behind");
                self.report(format!("{} fell too far behind", id.fmt_short()));
            }
        }
    }

    fn handle(&self, from: EndpointId, token: u64, message: Message) {
        match message {
            // A connection that was replaced, or one the user closed, can still
            // have a frame on the way; the editor has moved on from it.
            Message::Data(data) => {
                if self.member(from, token) {
                    self.emit(Event::Message { from, data })
                }
            }
            Message::Peers { peers } => self.meet(peers),
            Message::Hello { .. } | Message::Welcome { .. } => self.report(format!(
                "unexpected handshake message from {}",
                from.fmt_short()
            )),
        }
    }

    /// Dials the peers we are missing. Ourselves, a peer already connected and
    /// one already being dialed are all expected here: `dial` turns them away.
    fn meet(&self, peers: Vec<EndpointAddr>) {
        for addr in peers {
            let _ = self.dial(addr);
        }
    }

    /// Tells every peer who we are connected to.
    fn gossip(&self) {
        let state = self.state.lock().unwrap();
        // With one peer there is nobody to introduce it to.
        if state.peers.len() < 2 {
            return;
        }

        let addrs: Vec<_> = state.peers.values().map(|peer| peer.addr.clone()).collect();

        for (id, peer) in state.peers.iter() {
            let peers = addrs
                .iter()
                .filter(|addr| addr.id != *id)
                .cloned()
                .collect();
            self.post(*id, peer, Message::Peers { peers });
        }
    }
}

/// Whether a closed connection is one to open again. A peer that said goodbye,
/// and a connection dropped in favour of another one to the same peer, are not.
fn reconnectable(reason: Option<ConnectionError>) -> bool {
    match reason {
        // The stream failed under a connection that is still up, which leaves
        // the session with no way to talk over it.
        None => true,
        Some(ConnectionError::TimedOut | ConnectionError::Reset) => true,
        Some(ConnectionError::ApplicationClosed(close)) => {
            u64::from(close.error_code) == u64::from(OVERRUN)
        }
        Some(_) => false,
    }
}

impl ProtocolHandler for Session {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let remote = connection.remote_id();
        let generation = self.state.lock().unwrap().generation;

        if let Err(err) = self.answer(generation, connection).await {
            self.report(format!(
                "failed to accept connection from {}: {:#}",
                remote.fmt_short(),
                err
            ));
        }

        Ok(())
    }
}
