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

/// A message exchanged over the session stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Message {
    /// First frame of the stream, sent by the peer that dialed.
    Hello { addr: EndpointAddr },
    /// Answer to [`Message::Hello`], listing the rest of the session.
    Welcome { peers: Vec<EndpointAddr> },
    /// A peer that joined the session through us.
    PeerJoined { addr: EndpointAddr },
    /// Round trip probe. The nonce is only meaningful to its sender.
    Ping { nonce: u64 },
    /// Answer to [`Message::Ping`], echoing its nonce.
    Pong { nonce: u64 },
}

/// Encodes a message into a frame.
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

/// Decodes the body of a frame.
pub fn decode(body: &[u8]) -> Result<Message> {
    Ok(postcard::from_bytes(body)?)
}

/// Writes a single message to the stream.
pub async fn write(send: &mut SendStream, message: &Message) -> Result<()> {
    send.write_all(&encode(message)?).await?;
    Ok(())
}

/// Reads a single message from the stream.
///
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

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(message: Message) -> Message {
        let frame = encode(&message).unwrap();
        let length = u32::from_le_bytes(frame[..4].try_into().unwrap()) as usize;
        assert_eq!(length, frame.len() - 4);
        decode(&frame[4..]).unwrap()
    }

    #[test]
    fn roundtrips_a_ping() {
        let Message::Ping { nonce } = roundtrip(Message::Ping { nonce: 42 }) else {
            panic!("expected a ping");
        };
        assert_eq!(nonce, 42);
    }

    #[test]
    fn roundtrips_an_address() {
        let addr = EndpointAddr::new(iroh::SecretKey::from_bytes(&[7; 32]).public());

        let Message::Hello { addr: decoded } = roundtrip(Message::Hello { addr: addr.clone() })
        else {
            panic!("expected a hello");
        };
        assert_eq!(decoded, addr);
    }

    #[test]
    fn rejects_a_truncated_frame() {
        let frame = encode(&Message::Ping { nonce: 1 }).unwrap();
        assert!(decode(&frame[4..frame.len() - 1]).is_err());
    }
}
