pub mod proto;

use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
    sync::{Arc, Mutex},
};

use anyhow::{bail, ensure, Result};
use iroh::{
    endpoint::{presets, Connection, RecvStream, SendStream},
    protocol::{AcceptError, ProtocolHandler, Router},
    Endpoint, EndpointAddr, EndpointId,
};
use iroh_tickets::endpoint::EndpointTicket;
use tokio::sync::mpsc::{unbounded_channel, Sender, UnboundedReceiver, UnboundedSender};
use tokio_stream::wrappers::UnboundedReceiverStream;

use proto::Message;

pub const ALPN: &[u8] = b"helix/session/0";

const BYE: u32 = 0;

#[derive(Debug)]
pub enum Event {
    Connected(EndpointId),
    Disconnected(EndpointId),
    Message { from: EndpointId, message: Message },
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
    pub events: UnboundedReceiverStream<Event>,
    pub requests: UnboundedSender<Request>,
}

impl Service {
    pub fn new() -> Self {
        let (events_tx, events_rx) = unbounded_channel();
        let (requests_tx, mut requests_rx) = unbounded_channel();

        tokio::spawn(async move {
            let endpoint = Endpoint::builder(presets::N0)
                .bind()
                .await
                .expect("failed to bind the endpoint");
            endpoint.online().await;
            log::info!("listening as {}", endpoint.id().fmt_short());

            let node = Node::new(endpoint.clone(), events_tx);

            let _router = Router::builder(endpoint).accept(ALPN, node.clone()).spawn();

            while let Some(request) = requests_rx.recv().await {
                let result = match request {
                    Request::Ticket(chan) => {
                        let _ = chan.send(node.ticket()).await;
                        continue;
                    }
                    Request::Join(ticket) => node.join(&ticket),
                    Request::Peers(chan) => {
                        let _ = chan.send(node.peers()).await;
                        continue;
                    }
                    Request::Close => {
                        node.close();
                        continue;
                    }
                    Request::Broadcast(message) => node.broadcast(message),
                };

                if let Err(err) = result {
                    node.report(format!("{:#}", err));
                }
            }
        });

        Service {
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

#[derive(Debug)]
struct Peer {
    addr: EndpointAddr,
    connection: Connection,
    outbox: UnboundedSender<Message>,
}

/// The local node in the peer mesh. Cloned into every connection task.
#[derive(Debug, Clone)]
struct Node {
    endpoint: Endpoint,
    events: UnboundedSender<Event>,
    peers: Arc<Mutex<HashMap<EndpointId, Peer>>>,
    dialing: Arc<Mutex<HashSet<EndpointId>>>,
}

impl Node {
    fn new(endpoint: Endpoint, events: UnboundedSender<Event>) -> Self {
        Self {
            endpoint,
            events,
            peers: Arc::default(),
            dialing: Arc::default(),
        }
    }

    fn id(&self) -> EndpointId {
        self.endpoint.id()
    }

    fn emit(&self, event: Event) {
        let _ = self.events.send(event);
    }

    fn report(&self, error: String) {
        log::error!("{error}");
        self.emit(Event::Error(error));
    }

    fn ticket(&self) -> String {
        EndpointTicket::new(self.endpoint.addr()).to_string()
    }

    fn join(&self, ticket: &str) -> Result<()> {
        let ticket = EndpointTicket::from_str(ticket)?;
        let addr = ticket.endpoint_addr().clone();

        ensure!(addr.id != self.id(), "cannot join your own session");
        ensure!(
            !self.peers.lock().unwrap().contains_key(&addr.id),
            "already connected to {}",
            addr.id.fmt_short()
        );

        self.start_connect(addr);

        Ok(())
    }

    fn broadcast(&self, message: Message) -> Result<()> {
        let peers = self.peers.lock().unwrap();

        for peer in peers.values() {
            let _ = peer.outbox.send(message.clone());
        }
        Ok(())
    }

    fn peers(&self) -> Vec<EndpointId> {
        self.peers.lock().unwrap().keys().copied().collect()
    }

    fn close(&self) {
        let peers = std::mem::take(&mut *self.peers.lock().unwrap());
        for (id, peer) in peers {
            peer.connection.close(BYE.into(), b"bye");
            log::info!("disconnected from {}", id.fmt_short());
            self.emit(Event::Disconnected(id));
        }
    }

    fn start_connect(&self, addr: EndpointAddr) {
        let id = addr.id;
        if id == self.id()
            || self.peers.lock().unwrap().contains_key(&id)
            || !self.dialing.lock().unwrap().insert(id)
        {
            return;
        }

        let node = self.clone();
        tokio::spawn(async move {
            let result = node.connect(addr).await;
            node.dialing.lock().unwrap().remove(&id);
            if let Err(err) = result {
                node.report(format!(
                    "failed to connect to {}: {:#}",
                    id.fmt_short(),
                    err
                ));
            }
        });
    }

    async fn connect(&self, addr: EndpointAddr) -> Result<()> {
        let connection = self.endpoint.connect(addr.clone(), ALPN).await?;
        let (mut send, mut recv) = connection.open_bi().await?;

        proto::write(
            &mut send,
            &Message::Hello {
                addr: self.endpoint.addr(),
            },
        )
        .await?;

        let Some(Message::Welcome { peers }) = proto::read(&mut recv).await? else {
            bail!("expected a welcome");
        };

        for peer in peers {
            self.start_connect(peer);
        }

        self.serve(addr, connection, send, recv).await;

        Ok(())
    }

    async fn answer(&self, connection: Connection) -> Result<()> {
        let (mut send, mut recv) = connection.accept_bi().await?;

        let Some(Message::Hello { addr }) = proto::read(&mut recv).await? else {
            bail!("expected a hello");
        };
        ensure!(
            addr.id == connection.remote_id(),
            "hello address does not match the connection identity",
        );

        proto::write(
            &mut send,
            &Message::Welcome {
                peers: self.addrs(),
            },
        )
        .await?;

        self.serve(addr, connection, send, recv).await;

        Ok(())
    }

    async fn serve(
        &self,
        addr: EndpointAddr,
        connection: Connection,
        send: SendStream,
        mut recv: RecvStream,
    ) {
        let id = addr.id;
        let (outbox, queue) = unbounded_channel();

        self.peers.lock().unwrap().insert(
            addr.id,
            Peer {
                addr,
                connection: connection.clone(),
                outbox,
            },
        );
        self.start_writer(connection.clone(), send, queue);
        log::info!("connected to {}", id.fmt_short());
        self.emit(Event::Connected(id));

        loop {
            match proto::read(&mut recv).await {
                Ok(Some(message)) => self.handle(id, message),
                Ok(None) => break,
                Err(err) => {
                    if connection.close_reason().is_none() {
                        self.report(format!("failed to read from {}: {:#}", id.fmt_short(), err));
                    }
                    break;
                }
            }
        }

        if self.peers.lock().unwrap().remove(&id).is_some() {
            connection.close(BYE.into(), b"bye");
            log::info!("disconnected from {}", id.fmt_short());
            self.emit(Event::Disconnected(id));
        }
    }

    fn start_writer(
        &self,
        connection: Connection,
        mut send: SendStream,
        mut queue: UnboundedReceiver<Message>,
    ) {
        let node = self.clone();
        tokio::spawn(async move {
            let id = connection.remote_id();

            while let Some(message) = queue.recv().await {
                if let Err(err) = proto::write(&mut send, &message).await {
                    if connection.close_reason().is_none() {
                        node.report(format!("failed to write to {}: {:#}", id.fmt_short(), err));
                    }
                    break;
                }
            }

            let _ = send.finish();
        });
    }

    fn handle(&self, from: EndpointId, message: Message) {
        match message {
            Message::Share { .. } | Message::Edit { .. } => {
                self.emit(Event::Message { from, message })
            }
            Message::Hello { .. } | Message::Welcome { .. } => self.report(format!(
                "unexpected handshake message from {}",
                from.fmt_short()
            )),
        }
    }

    fn addrs(&self) -> Vec<EndpointAddr> {
        self.peers
            .lock()
            .unwrap()
            .values()
            .map(|peer| peer.addr.clone())
            .collect()
    }
}

impl ProtocolHandler for Node {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let remote = connection.remote_id();

        if let Err(err) = self.answer(connection).await {
            self.report(format!(
                "failed to accept connection from {}: {:#}",
                remote.fmt_short(),
                err
            ));
        }

        Ok(())
    }
}
