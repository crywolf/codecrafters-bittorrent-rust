use std::ops::{Deref, DerefMut};

use anyhow::Context;
use futures_util::{SinkExt, StreamExt};
use num_enum::{IntoPrimitive, TryFromPrimitive};
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio_util::codec::Framed;

use super::codec::MessageCodec;
use crate::torrent::Info as TorrentInfo;

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

/// https://www.bittorrent.org/beps/bep_0009.html#extension-message
#[derive(Serialize, Deserialize, IntoPrimitive, TryFromPrimitive, Debug)]
#[repr(u8)]
enum ExtendedMessageType {
    Request = 0,
    Data = 1,
    Reject = 2,
}

const METADATA_EXTENSION_ID: u8 = 2; // arbitrary number

/// Message codec
#[derive(Debug)]
pub struct Framer(
    Framed<TcpStream, MessageCodec>,
    Option<u8>,          // peer_metadata_extension_id
    Option<TorrentInfo>, // torrent info from magnet metadata extension
);

impl Framer {
    pub async fn new(peer_stream: TcpStream, support_extensions: bool) -> anyhow::Result<Self> {
        let mut framer = Framed::new(peer_stream, MessageCodec);

        // 1. Send the bitfield message (safe to ignore in this challenge)

        // 2. Wait for a Bitfield message from the peer indicating which pieces it has
        //    Ignore the payload, the tracker used for this challenge ensures that all peers have all pieces available
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
        eprintln!("Received Bitfield message");

        // Extension IDs need to be stored for every peer, as different peers may use different IDs for the same extension.
        let mut stored_peer_metadata_extension_id = None;

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
                .context("decoding Extended message handshake")?;

            anyhow::ensure!(
                extended_msg.tag == MessageTag::Extended,
                "Expected Extended handshake message, got {:?}",
                extended_msg.tag
            );

            let extended_msg_id = extended_msg.payload[0];
            anyhow::ensure!(
                extended_msg_id == handshake_extended_msg_id,
                "Expected hanshake extended message ID should be 0, got {}",
                extended_msg_id
            );

            let ext_payload: ExtendedMsgPayload =
                serde_bencode::from_bytes(&extended_msg.payload[1..])
                    .context("deserializing Extended msg payload")?;

            let peer_metadata_extension_id = ext_payload.m.ut_metadata;
            stored_peer_metadata_extension_id = Some(peer_metadata_extension_id);
            eprintln!(
                "Received Extended handshake message response (Peer Metadata Extension ID: {})",
                peer_metadata_extension_id
            );
        }

        Ok(Self(framer, stored_peer_metadata_extension_id, None))
    }

    pub async fn magnet_torrent_info(&mut self) -> anyhow::Result<Option<TorrentInfo>> {
        if self.1.is_none() {
            anyhow::bail!("Peer does not support metadata extensions")
        }
        if self.2.is_some() {
            return Ok(self.2.clone());
        }

        eprintln!("Sending Metadata extension Request message");
        // 2C. Send the metadata request message
        //     bencoded dictionary: {'msg_type': 0, 'piece': 0}
        //      msg_type will be 0 since this is a request message
        //      piece is the zero-based piece index of the metadata being requested
        //         (since we're only requesting one piece in this challenge, this will always be 0)
        // https://www.bittorrent.org/beps/bep_0009.html#extension-message
        #[derive(Serialize)]
        struct MetadataRequest {
            pub msg_type: u8,
            pub piece: usize,
        }
        let metadata_request = MetadataRequest {
            msg_type: ExtendedMessageType::Request.into(), // request message
            piece: 0,
        };
        let request_payload = serde_bencode::to_bytes(&metadata_request)
            .context("bencoding metadata request payload")?;

        let mut payload = vec![self
            .peer_metadata_extension_id()
            .expect("peer_metadata_extension_id was already set in constructor")];
        payload.extend_from_slice(&request_payload);

        self.send(Message {
            tag: MessageTag::Extended,
            payload,
        })
        .await
        .context("sending Extended message (metadata request)")?;
        eprintln!("Metadata extension Request message sent");

        eprintln!("Waiting for Metadata extension Data message");
        // 2D. Wait for the metadata message
        //     bencoded dictionary: {'msg_type': 1, 'piece': 0, 'total_size': 3425}
        //      msg_type will be 1, since this is a data message
        //      piece will be 0, since we're only requesting one piece in this challenge
        //      total_size will be the length of the metadata piece
        //
        // d8:msg_typei1e5:piecei0e10:total_sizei34256eexxxxxxxx...
        // The x represents binary data (the metadata).

        #[derive(Deserialize, Debug)]
        #[allow(dead_code)]
        struct MetadataResponse {
            pub msg_type: u8,
            pub piece: usize,
            pub total_size: usize,
        }

        let extended_msg = self
            .next()
            .await
            .expect("expecting Extended message (metadata response)")
            .context("decoding Extended message")?;

        anyhow::ensure!(
            extended_msg.tag == MessageTag::Extended,
            "Expected Extended Metadata message, got {:?}",
            extended_msg.tag
        );

        let extended_msg_id = extended_msg.payload[0];
        anyhow::ensure!(
            extended_msg_id == METADATA_EXTENSION_ID,
            "Expected extended message ID (metadata response) should be {}, got {}",
            METADATA_EXTENSION_ID,
            extended_msg_id
        );
        let response: MetadataResponse = serde_bencode::from_bytes(&extended_msg.payload[1..])
            .context("deserializing Extended msg payload")?;

        let mut torrent_info: TorrentInfo = serde_bencode::from_bytes(
            &extended_msg.payload[extended_msg.payload.len() - response.total_size..],
        )
        .context("deserializing Extended Metadata msg payload")?;

        torrent_info
            .finalize()
            .context("finalize deserialized torrent info")?;

        self.2 = Some(torrent_info);

        Ok(self.2.clone())
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
