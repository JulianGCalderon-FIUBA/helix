pub mod proto;
mod session;

use iroh::{endpoint::presets, protocol::Router, Endpoint, EndpointId};
use tokio::sync::mpsc::{unbounded_channel, Sender, UnboundedSender};
use tokio_stream::wrappers::UnboundedReceiverStream;

use proto::Message;
use session::Session;

pub const ALPN: &[u8] = b"helix/session/0";

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

            let session = Session::new(endpoint.clone(), events_tx);

            let _router = Router::builder(endpoint)
                .accept(ALPN, session.clone())
                .spawn();

            while let Some(request) = requests_rx.recv().await {
                let result = match request {
                    Request::Ticket(chan) => {
                        let _ = chan.send(session.ticket()).await;
                        continue;
                    }
                    Request::Join(ticket) => session.join(&ticket),
                    Request::Peers(chan) => {
                        let _ = chan.send(session.peers()).await;
                        continue;
                    }
                    Request::Close => {
                        session.close();
                        continue;
                    }
                    Request::Broadcast(message) => session.broadcast(message),
                };

                if let Err(err) = result {
                    session.report(format!("{:#}", err));
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
