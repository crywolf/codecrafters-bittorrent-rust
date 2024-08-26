use std::ops::{Deref, DerefMut};

use anyhow::Context;
use futures_util::{SinkExt, StreamExt};
use num_enum::{IntoPrimitive, TryFromPrimitive};
use tokio::net::TcpStream;
use tokio_util::codec::Framed;

use super::codec::MessageCodec;

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoPrimitive, TryFromPrimitive)]
#[repr(u8)]
pub enum MessageTag {
    Choke = 0,
    Unchoke = 1,
    Interested = 2,
    NotInterested = 3,
    Have = 4,
    Bitfield = 5,
    Request = 6,
    Piece = 7,
    Cancel = 8,
}

#[derive(Debug)]
pub struct Message {
    pub tag: MessageTag,
    pub payload: Vec<u8>,
}

#[derive(Debug)]
pub struct Framer(Framed<TcpStream, MessageCodec>);

impl Framer {
    pub async fn new(peer_stream: TcpStream) -> anyhow::Result<Self> {
        let mut framer = Framed::new(peer_stream, MessageCodec);

        // 1. Wait for a Bitfield message from the peer indicating which pieces it has
        let bitfield_msg = framer
            .next()
            .await
            .expect("expecting Bitfield message")
            .context("decoding Bitfield message")?;

        anyhow::ensure!(bitfield_msg.tag == MessageTag::Bitfield, "Bitfield message");
        // ignore the payload for now, the tracker used for this challenge ensures that all peers have all pieces available

        // 2. Send an Interested message
        framer
            .send(Message {
                tag: MessageTag::Interested,
                payload: Vec::new(),
            })
            .await
            .context("sending Interested message")?;

        // 3. Wait until you receive an Unchoke message back
        let unchoke_msg = framer
            .next()
            .await
            .expect("expecting Unchoke message")
            .context("decoding Unchoke message")?;

        anyhow::ensure!(unchoke_msg.tag == MessageTag::Unchoke, "Unchoke message");

        Ok(Self(framer))
    }
}

impl Deref for Framer {
    type Target = Framed<TcpStream, MessageCodec>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for Framer {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
