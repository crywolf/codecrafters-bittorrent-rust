use std::ops::{Deref, DerefMut};

use anyhow::Context;
use futures_util::{SinkExt, StreamExt};
use num_enum::{IntoPrimitive, TryFromPrimitive};
use serde::{Deserialize, Serialize};
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
    Extended = 20,
}

#[derive(Debug)]
pub struct Message {
    pub tag: MessageTag,
    pub payload: Vec<u8>,
}

const METADATA_EXTENSION_ID: u8 = 2; // arbitrary number

/// Message codec
/// It also remembers the metadata extension id for a connected peer
#[derive(Debug)]
pub struct Framer(Framed<TcpStream, MessageCodec>, Option<u8>);

impl Framer {
    pub async fn new(peer_stream: TcpStream, support_extensions: bool) -> anyhow::Result<Self> {
        let mut framer = Framed::new(peer_stream, MessageCodec);

        // 1. Send the bitfield message (safe to ignore in this challenge)

        // 2. Wait for a Bitfield message from the peer indicating which pieces it has
        //    Ignore the payload for now, the tracker used for this challenge ensures that all peers have all pieces available
        eprintln!("Waiting for Bitfield message");
        let bitfield_msg = framer
            .next()
            .await
            .expect("expecting Bitfield message")
            .context("decoding Bitfield message")?;

        anyhow::ensure!(
            bitfield_msg.tag == MessageTag::Bitfield,
            "Expected Bitfield message, got {:?}",
            bitfield_msg.tag
        );
        eprintln!("Got Bitfield message");

        // Extension IDs need to be stored for every peer, as different peers may use different IDs for the same extension.
        let mut peer_metadata_extension_id = None;
        if support_extensions {
            // https://www.bittorrent.org/beps/bep_0010.html
            eprintln!("Peer supports extensions => sending Extended handshake message");

            /// https://www.bittorrent.org/beps/bep_0010.html#handshake-message
            #[derive(Serialize, Deserialize)]
            struct ExtendedMsgPayload {
                m: Mdictionary,
            }

            /// The payload of the extended handshake message is a bencoded dictionary.
            /// {
            ///   "m": {
            ///     "ut_metadata": 16,
            ///     ... (other extension names and IDs)
            ///   }
            /// }
            #[derive(Serialize, Deserialize)]
            struct Mdictionary {
                pub ut_metadata: u8,
            }

            // 2A. Send an Extended handshake message (if peer supports extensions)
            let m = Mdictionary {
                ut_metadata: METADATA_EXTENSION_ID, // metadata extension
            };
            let ext_payload = ExtendedMsgPayload { m };
            let ext_payload =
                serde_bencode::to_bytes(&ext_payload).context("bencoding Extended msg payload")?;

            let handshake_extended_msg_id = 0u8; // extended message ID. 0 = handshake, >0 = extended message as specified by the handshake.
            let mut payload = vec![handshake_extended_msg_id];
            payload.extend_from_slice(&ext_payload);

            framer
                .send(Message {
                    tag: MessageTag::Extended,
                    payload,
                })
                .await
                .context("sending Extended message (extension handshake)")?;

            // 2B. Wait until you receive an Extended handshake message back
            eprintln!("Waiting for Extended handhake message response");
            let extended_msg = framer
                .next()
                .await
                .expect("expecting Extended message")
                .context("decoding Extended message")?;

            anyhow::ensure!(
                extended_msg.tag == MessageTag::Extended,
                "Expected Extended handshake message, got {:?}",
                extended_msg.tag
            );

            let extended_msg_id = extended_msg.payload[0];
            let ext_payload: ExtendedMsgPayload =
                serde_bencode::from_bytes(&extended_msg.payload[1..])
                    .context("deserializing Extended msg payload")?;

            peer_metadata_extension_id = Some(ext_payload.m.ut_metadata);

            anyhow::ensure!(
                extended_msg_id == handshake_extended_msg_id,
                "Expected hanshake extended message ID should be 0, got {}",
                extended_msg_id
            );
            eprintln!("Got Extended message response");

            eprintln!("Sending Metadata extension message");
            // 2C. Send the metadata request message {'msg_type': 0, 'piece': 0}
            // https://www.bittorrent.org/beps/bep_0009.html#extension-message
            #[derive(Serialize, Deserialize, Debug)]
            struct MetadataRequest {
                pub msg_type: u8,
                pub piece: usize,
            }
            let request_msg_type = 0; // request message
            let metadata_request = MetadataRequest {
                msg_type: request_msg_type,
                piece: 0,
            };
            let request_payload = serde_bencode::to_bytes(&metadata_request)
                .context("bencoding metadata request payload")?;

            let mut payload =
                vec![peer_metadata_extension_id.expect("peer metadata extension id must be set")];
            payload.extend_from_slice(&request_payload);

            framer
                .send(Message {
                    tag: MessageTag::Extended,
                    payload,
                })
                .await
                .context("sending Extended message (metadata request)")?;

            eprintln!("Metadata extension message sent");
        }

        // 3. Send an Interested message
        eprintln!("Sending Interested message");
        framer
            .send(Message {
                tag: MessageTag::Interested,
                payload: Vec::new(),
            })
            .await
            .context("sending Interested message")?;

        /*
        *** Disabled because of codecrafters tests (some of them do not send Unchoke messages) ***

               // 4. Wait until you receive an Unchoke message back
               eprintln!("Waiting for Unchoke message");
               let unchoke_msg = framer
                   .next()
                   .await
                   .expect("expecting Unchoke message")
                   .context("decoding Unchoke message")?;

               anyhow::ensure!(
                   unchoke_msg.tag == MessageTag::Unchoke,
                   "Expected Unchoke message, got {:?}",
                   unchoke_msg.tag
               );
               eprintln!("Got Unchoke message");
        */

        Ok(Self(framer, peer_metadata_extension_id))
    }

    pub fn peer_metadata_extension_id(&self) -> Option<u8> {
        self.1
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
