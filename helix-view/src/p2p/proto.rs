//! Wire format spoken between the peers of a collaboration session.
//!
//! Every connection carries a single bidirectional QUIC stream, the *session
//! stream*, over which both peers exchange a sequence of [`Message`] frames.
//! A frame is a little endian `u32` length followed by that many bytes of
//! postcard. Using one long lived stream keeps the messages of a peer in the
//! order it sent them, which a stream per message would not.

use anyhow::{ensure, Result};
use iroh::{
    endpoint::{ReadExactError, RecvStream, SendStream},
    EndpointAddr,
};
use serde::{Deserialize, Serialize};

/// Largest frame we are willing to write or read.
///
/// Peers are not trusted to send a sane length, so the limit is also what
/// keeps a malformed frame from allocating an arbitrary amount of memory.
const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Message {
    /// First frame of the stream, sent by the peer that dialed. The address
    /// is the dialer's own view of itself, which is what a third peer needs
    /// to reach it: the one we observe on the connection may be a path only
    /// we can use.
    Hello { addr: EndpointAddr },
    /// Answer to [`Message::Hello`], listing the rest of the session.
    Welcome { peers: Vec<EndpointAddr> },
    /// A payload for the layer above, which this one does not interpret.
    Data(Vec<u8>),
}

pub fn encode(message: &Message) -> Result<Vec<u8>> {
    let body = postcard::to_stdvec(message)?;
    ensure!(
        body.len() <= MAX_FRAME_SIZE,
        "message of {} bytes exceeds the {} byte limit",
        body.len(),
        MAX_FRAME_SIZE
    );

    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
    frame.extend_from_slice(&body);

    Ok(frame)
}

pub fn decode(body: &[u8]) -> Result<Message> {
    Ok(postcard::from_bytes(body)?)
}

pub async fn write(send: &mut SendStream, message: &Message) -> Result<()> {
    send.write_all(&encode(message)?).await?;
    Ok(())
}

/// Returns `None` once the remote finished the stream, which is how a peer
/// that leaves cleanly ends the conversation.
pub async fn read(recv: &mut RecvStream) -> Result<Option<Message>> {
    let mut length = [0; 4];
    match recv.read_exact(&mut length).await {
        Ok(()) => {}
        Err(ReadExactError::FinishedEarly(0)) => return Ok(None),
        Err(err) => return Err(err.into()),
    }

    let length = u32::from_le_bytes(length) as usize;
    ensure!(
        length <= MAX_FRAME_SIZE,
        "frame of {} bytes exceeds the {} byte limit",
        length,
        MAX_FRAME_SIZE
    );

    let mut body = vec![0; length];
    recv.read_exact(&mut body).await?;

    decode(&body).map(Some)
}
