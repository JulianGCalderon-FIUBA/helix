use std::{
    str::FromStr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{ensure, Context};
use iroh::{
    endpoint::{presets, Connection},
    protocol::{AcceptError, ProtocolHandler, Router},
    Endpoint, PublicKey,
};
use iroh_tickets::endpoint::EndpointTicket;
use tokio::sync::mpsc::{unbounded_channel, Sender, UnboundedSender};
use tokio_stream::wrappers::UnboundedReceiverStream;

pub const ALPN: &[u8] = b"helix/session/0";

const PING: &[u8] = b"PING";
const PONG: &[u8] = b"PONG";

const BYE: u32 = 0;

/// A long lived session with a single peer.
#[derive(Debug, Clone)]
struct Session {
    endpoint: Endpoint,
    events: UnboundedSender<Event>,
    peer: Arc<Mutex<Option<Connection>>>,
}

impl Session {
    fn connection(&self) -> Option<Connection> {
        self.peer.lock().unwrap().clone()
    }

    fn enter(&self, connection: &Connection) {
        *self.peer.lock().unwrap() = Some(connection.clone());
    }

    fn take(&self) -> Option<Connection> {
        self.peer.lock().unwrap().take()
    }

    async fn ticket_new(&self, chan: Sender<String>) -> anyhow::Result<()> {
        let ticket = EndpointTicket::new(self.endpoint.addr());
        chan.send(ticket.to_string()).await.unwrap();
        Ok(())
    }

    async fn ticket_join(&self, ticket: &str) -> anyhow::Result<()> {
        let ticket = EndpointTicket::from_str(ticket)?;
        let connection = self.endpoint.connect(ticket, ALPN).await?;
        let session = self.clone();
        tokio::spawn(async move { session.accept(connection).await });
        Ok(())
    }

    fn ticket_ping(&self) -> anyhow::Result<()> {
        let connection = self.connection().context("not in a session")?;
        let remote = connection.remote_id();
        let events = self.events.clone();

        tokio::spawn(async move {
            match send_ping(&connection).await {
                Ok(rtt) => {
                    events.send(Event::Pong(remote, rtt)).unwrap();
                }
                Err(err) => {
                    events
                        .send(Event::Error(format!(
                            "failed to ping {}: {:#}",
                            remote.fmt_short(),
                            err
                        )))
                        .unwrap();
                }
            }
        });

        Ok(())
    }

    fn ticket_close(&self) -> anyhow::Result<()> {
        let connection = self.take().context("not in a session")?;
        let remote = connection.remote_id();

        connection.close(BYE.into(), b"bye");
        self.events.send(Event::Disconnected(remote)).unwrap();

        Ok(())
    }
}

impl ProtocolHandler for Session {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let remote = connection.remote_id();
        self.enter(&connection);

        self.events.send(Event::Connected(remote)).unwrap();

        loop {
            if let Err(err) = answer_ping(&connection).await {
                if connection.close_reason().is_none() {
                    self.events
                        .send(Event::Error(format!(
                            "failed to answer ping from {}: {:#}",
                            remote.fmt_short(),
                            err
                        )))
                        .unwrap();
                }
                break;
            }

            self.events.send(Event::Ping(remote)).unwrap();
        }

        if let Some(connection) = self.take() {
            connection.close(BYE.into(), b"bye");
            self.events.send(Event::Disconnected(remote)).unwrap();
        }

        Ok(())
    }
}

async fn send_ping(connection: &Connection) -> anyhow::Result<Duration> {
    let start = Instant::now();

    let (mut send, mut recv) = connection.open_bi().await?;
    send.write_all(PING).await?;
    send.finish()?;

    let response = recv.read_to_end(PONG.len()).await?;
    ensure!(response == PONG, "expected a pong response");

    Ok(start.elapsed())
}

async fn answer_ping(connection: &Connection) -> anyhow::Result<()> {
    let (mut send, mut recv) = connection.accept_bi().await?;

    let request = recv.read_to_end(PING.len()).await?;
    ensure!(request == PING, "expected a ping request");

    send.write_all(PONG).await?;
    send.finish()?;

    Ok(())
}

pub struct Service {
    pub incoming: UnboundedReceiverStream<Event>,
    pub server_tx: UnboundedSender<Payload>,
}

#[derive(Debug)]
pub enum Event {
    Connected(PublicKey),
    Ping(PublicKey),
    Pong(PublicKey, Duration),
    Disconnected(PublicKey),
    Error(String),
}

#[derive(Debug)]
pub enum Payload {
    TicketNew(Sender<String>),
    TicketJoin(String),
    TicketPing,
    TicketClose,
}

impl Service {
    pub fn new() -> Self {
        let (events_tx, events_rx) = unbounded_channel();
        let (payloads_tx, mut payloads_rx) = unbounded_channel();

        tokio::spawn(async move {
            let endpoint = Endpoint::builder(presets::N0).bind().await.unwrap();
            endpoint.online().await;

            let session = Session {
                endpoint: endpoint.clone(),
                events: events_tx,
                peer: Arc::default(),
            };

            let _router = Router::builder(endpoint)
                .accept(ALPN, session.clone())
                .spawn();

            while let Some(payload) = payloads_rx.recv().await {
                let result = match payload {
                    Payload::TicketNew(chan) => session.ticket_new(chan).await,
                    Payload::TicketJoin(ticket) => session.ticket_join(&ticket).await,
                    Payload::TicketPing => session.ticket_ping(),
                    Payload::TicketClose => session.ticket_close(),
                };

                if let Err(err) = result {
                    session
                        .events
                        .send(Event::Error(format!("{:#}", err)))
                        .unwrap();
                }
            }
        });

        Service {
            incoming: UnboundedReceiverStream::new(events_rx),
            server_tx: payloads_tx,
        }
    }
}

impl Default for Service {
    fn default() -> Self {
        Self::new()
    }
}
