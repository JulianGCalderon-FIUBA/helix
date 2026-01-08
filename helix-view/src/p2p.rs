use iroh::{
    discovery::mdns::{self, MdnsDiscovery},
    endpoint::Connection,
    protocol::{AcceptError, ProtocolHandler, Router},
    Endpoint, EndpointAddr,
};
use log::{error, info};
use n0_future::StreamExt;

pub const ALPN: &[u8] = b"helix/ping/0";

#[derive(Debug, Clone)]
pub struct PingPong {
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

        send.write_all(b"PONG")
            .await
            .map_err(AcceptError::from_err)?;
        info!("ponging: {}", connection.remote_id().fmt_short());

        send.finish()?;
        connection.closed().await;

        Ok(())
    }
}

pub struct Service {}

impl Service {
    pub fn new() -> Self {
        tokio::spawn(async move {
            let endpoint = Endpoint::builder()
                .bind()
                .await
                .expect("failed to bind endpoint");
            info!("binded at: {}", endpoint.id().fmt_short());

            let pingpong = PingPong {
                endpoint: endpoint.clone(),
            };

            let _router = Router::builder(endpoint.clone())
                .accept(ALPN, pingpong.clone())
                .spawn();

            let mdns = MdnsDiscovery::builder()
                .build(endpoint.id())
                .expect("failed to build discovery service");
            endpoint.discovery().add(mdns.clone());

            let mut discovery_events = mdns.subscribe().await;
            while let Some(event) = discovery_events.next().await {
                match event {
                    mdns::DiscoveryEvent::Discovered { endpoint_info, .. } => {
                        info!("peer discovered: {}", endpoint_info.endpoint_id.fmt_short());

                        if let Err(err) = pingpong.ping(endpoint_info.endpoint_id).await {
                            error!("failed to ping: {:#}", err);
                        }
                    }
                    mdns::DiscoveryEvent::Expired { endpoint_id } => {
                        error!("peer expired: {}", endpoint_id.fmt_short())
                    }
                }
            }
        });

        Service {}
    }
}

impl Default for Service {
    fn default() -> Self {
        Self::new()
    }
}
