use std::str::FromStr;

use iroh::{
    endpoint::Connection,
    protocol::{AcceptError, ProtocolHandler, Router},
    Endpoint, EndpointAddr, PublicKey,
};
use iroh_tickets::endpoint::EndpointTicket;
use log::{error, info};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio_stream::wrappers::UnboundedReceiverStream;

pub const ALPN: &[u8] = b"helix/ping/0";

#[derive(Debug, Clone)]
struct PingPong {
    client_tx: UnboundedSender<Event>,
    endpoint: Endpoint,
}

impl PingPong {
    async fn ping(&self, addr: impl Into<EndpointAddr>) -> anyhow::Result<()> {
        let connection = self.endpoint.connect(addr, ALPN).await?;
        let (mut send, mut recv) = connection.open_bi().await?;

        send.write_all(b"PING").await?;
        send.finish()?;
        info!("pinging: {}", connection.remote_id().fmt_short());

        let response = recv.read_to_end(4).await?;
        assert_eq!(&response, b"PONG");
        info!("ponged by: {}", connection.remote_id().fmt_short());

        connection.close(0u32.into(), b"bye!");

        Ok(())
    }
}

impl ProtocolHandler for PingPong {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let (mut send, mut recv) = connection.accept_bi().await?;

        let req = recv.read_to_end(4).await.map_err(AcceptError::from_err)?;
        assert_eq!(&req, b"PING");
        info!("pinged by: {}", connection.remote_id().fmt_short());

        self.client_tx
            .send(Event::Ping(connection.remote_id()))
            .unwrap();

        send.write_all(b"PONG")
            .await
            .map_err(AcceptError::from_err)?;
        info!("ponging: {}", connection.remote_id().fmt_short());

        send.finish()?;
        connection.closed().await;

        Ok(())
    }
}

pub struct Service {
    pub incoming: UnboundedReceiverStream<Event>,
    pub server_tx: UnboundedSender<Payload>,
}

#[derive(Debug)]
pub enum Event {
    Ping(PublicKey),
}

#[derive(Debug)]
pub enum Payload {
    TicketNew,
    TicketJoin(String),
}

impl Service {
    pub fn new() -> Self {
        let (server_tx, client_rx) = unbounded_channel();
        let (client_tx, mut server_rx) = unbounded_channel();

        tokio::spawn(async move {
            let endpoint = Endpoint::builder()
                .bind()
                .await
                .expect("failed to bind endpoint");
            info!("binded at {}", endpoint.id().fmt_short());

            let pingpong = PingPong {
                endpoint: endpoint.clone(),
                client_tx: server_tx,
            };

            let _router = Router::builder(endpoint.clone())
                .accept(ALPN, pingpong.clone())
                .spawn();

            while let Some(payload) = server_rx.recv().await {
                match payload {
                    Payload::TicketNew => {
                        let ticket = EndpointTicket::new(endpoint.addr());
                        info!("generated ticket {}", ticket);
                    }
                    Payload::TicketJoin(ticket) => {
                        let ticket = EndpointTicket::from_str(&ticket)
                            .expect("failed to deserialize ticket");
                        if let Err(err) = pingpong.ping(ticket.clone()).await {
                            error!(
                                "failed to ping peer {}: {:#}",
                                ticket.endpoint_addr().id.fmt_short(),
                                err
                            )
                        }
                    }
                }
            }
        });

        Service {
            incoming: UnboundedReceiverStream::new(client_rx),
            server_tx: client_tx,
        }
    }
}

impl Default for Service {
    fn default() -> Self {
        Self::new()
    }
}
