use iroh::{
    discovery::mdns::{self, MdnsDiscovery},
    endpoint::Connection,
    protocol::{AcceptError, ProtocolHandler, Router},
    Endpoint, EndpointAddr, PublicKey,
};
use log::{error, info, warn};
use n0_future::StreamExt;
use rand::{rngs::StdRng, seq::IteratorRandom, SeedableRng};
use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio_stream::wrappers::UnboundedReceiverStream;

pub const ALPN: &[u8] = b"helix/ping/0";

#[derive(Debug, Clone)]
pub struct PingPong {
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
    RandomPing,
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

            let mdns = MdnsDiscovery::builder()
                .build(endpoint.id())
                .expect("failed to build discovery service");
            endpoint.discovery().add(mdns.clone());

            let peers = Arc::new(Mutex::new(HashSet::new()));
            let peers_clone = peers.clone();
            tokio::spawn(async move {
                let mut discovery_events = mdns.subscribe().await;

                while let Some(event) = discovery_events.next().await {
                    match event {
                        mdns::DiscoveryEvent::Discovered { endpoint_info, .. } => {
                            let unknown = peers_clone
                                .lock()
                                .unwrap()
                                .insert(endpoint_info.endpoint_id);
                            if unknown {
                                info!("discovered peer {}", endpoint_info.endpoint_id.fmt_short());
                            }
                        }
                        mdns::DiscoveryEvent::Expired { endpoint_id } => {
                            warn!("expired peer {}", endpoint_id.fmt_short());
                            peers_clone.lock().unwrap().remove(&endpoint_id);
                        }
                    }
                }
            });

            let mut rng = { StdRng::from_rng(&mut rand::rng()) };
            while let Some(payload) = server_rx.recv().await {
                match payload {
                    Payload::RandomPing => {
                        let peer = peers.lock().unwrap().iter().choose(&mut rng).cloned();

                        match peer {
                            Some(peer) => {
                                if let Err(err) = pingpong.ping(peer).await {
                                    error!("failed to ping peer {}: {:#}", peer.fmt_short(), err)
                                }
                            }
                            None => {
                                warn!("no peers to ping")
                            }
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
