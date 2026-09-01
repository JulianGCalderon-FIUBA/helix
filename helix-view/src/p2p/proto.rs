use anyhow::{ensure, Result};
use helix_core::crdt::Op;
use iroh::{
    endpoint::{ReadExactError, RecvStream, SendStream},
    EndpointAddr,
};
use serde::{Deserialize, Serialize};

const MAX_BODY_SIZE: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Message {
    Hello {
        addr: EndpointAddr,
    },
    Welcome {
        peers: Vec<EndpointAddr>,
    },
    /// The initial handoff: the sharer's buffer, plus the encoded replica the
    /// receiver forks its own from.
    Share {
        text: String,
        replica: Vec<u8>,
    },
    Edit(Op),
}

pub fn encode(message: &Message) -> Result<Vec<u8>> {
    let body = postcard::to_stdvec(message)?;
    ensure!(body.len() <= MAX_BODY_SIZE, "message is too big");

    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
    frame.extend_from_slice(&body);

    Ok(frame)
}

pub fn decode_body(body: &[u8]) -> Result<Message> {
    Ok(postcard::from_bytes(body)?)
}

pub async fn write(send: &mut SendStream, message: &Message) -> Result<()> {
    send.write_all(&encode(message)?).await?;
    Ok(())
}

pub async fn read(recv: &mut RecvStream) -> Result<Option<Message>> {
    let mut length = [0; 4];
    match recv.read_exact(&mut length).await {
        Ok(()) => {}
        Err(ReadExactError::FinishedEarly(0)) => return Ok(None),
        Err(err) => return Err(err.into()),
    }

    let length = u32::from_le_bytes(length) as usize;
    ensure!(length <= MAX_BODY_SIZE, "body is too big");

    let mut body = vec![0; length];
    recv.read_exact(&mut body).await?;

    decode_body(&body).map(Some)
}
