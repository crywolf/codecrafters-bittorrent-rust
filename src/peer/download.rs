use std::path::Path;

use anyhow::Context;
use bytes::{Buf, BufMut};
use futures_util::SinkExt;
use futures_util::StreamExt;
use num_enum::{IntoPrimitive, TryFromPrimitive};
use sha1::Digest;

use crate::peer::codec::MessageCodec;
use crate::torrent::Torrent;

const BLOCK_MAX_SIZE: usize = 1 << 14; // 2^14 (16 kiB)

// Requests are pipelined to improve download speeds.
// BitTorrent Economics Paper [http://bittorrent.org/bittorrentecon.pdf] recommends having 5 requests pending at once,
// to avoid a delay between blocks being sent.
const PIPELINED_REQUESTS: usize = 5;

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

pub async fn download_piece(
    stream: tokio::net::TcpStream,
    output: impl AsRef<Path>,
    piece: usize,
    torrent: &Torrent,
) -> anyhow::Result<()> {
    let pieces_count = torrent.info.hashes.len();

    if piece > pieces_count {
        anyhow::bail!("piece argument out of range, max value is {}", pieces_count)
    }

    let mut framer = tokio_util::codec::Framed::new(stream, MessageCodec);

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

    let file_len = torrent.info.length;

    let piece_len = if piece == pieces_count - 1 {
        let md = file_len % torrent.info.piece_length;
        if md == 0 {
            torrent.info.piece_length
        } else {
            md
        }
    } else {
        torrent.info.piece_length
    };

    let blocks_count = (piece_len + (BLOCK_MAX_SIZE - 1)) / BLOCK_MAX_SIZE; // round up

    let (request_tx, mut request_rx) = tokio::sync::mpsc::channel::<Message>(PIPELINED_REQUESTS);

    tokio::spawn(async move {
        for block in 0..blocks_count {
            let mut payload = Vec::new();
            payload.put_u32(piece as u32); // the zero-based piece index

            let begin = block * BLOCK_MAX_SIZE;
            payload.put_u32(begin as u32); // the zero-based byte offset within the piece (this'll be 0 for the first block, 2^14 for the second block, 2*2^14 for the third block etc.)

            let block_len = if block == blocks_count - 1 {
                let md = piece_len % BLOCK_MAX_SIZE;
                if md == 0 {
                    BLOCK_MAX_SIZE
                } else {
                    md
                }
            } else {
                BLOCK_MAX_SIZE
            };
            payload.put_u32(block_len as u32); // the length of the block in bytes

            let message = Message {
                tag: MessageTag::Request,
                payload,
            };

            request_tx
                .send(message)
                .await
                .expect("receiver should not be closed");
        }
    });

    let (response_tx, mut response_rx) = tokio::sync::mpsc::channel::<Message>(1);

    tokio::spawn(async move {
        loop {
            tokio::select! {
                // 4. Send a Request message for each block
                Some(message) = request_rx.recv() => {
                    _ = framer
                        .send(message)
                        .await
                        .context("sending Request message") .map_err(|e| eprintln!("Error: {:?}", e));
                    }

                // 5. Wait for a Piece message for each block you've requested
                Some(Ok(message)) = framer.next() => {
                    if message.tag == MessageTag::Piece { // ignoring other message types
                        response_tx
                        .send(message)
                        .await
                        .expect("receiver should not be closed")
                    }
                }
            }
        }
    });

    let mut piece_buf: Vec<Vec<u8>> = Vec::with_capacity(blocks_count);
    for _ in 0..blocks_count {
        piece_buf.push(Vec::with_capacity(BLOCK_MAX_SIZE));
    }

    let mut received_blocks = 0;
    while let Some(piece_msg) = response_rx.recv().await {
        let mut piece_msg_paylod = piece_msg.payload.as_slice();
        let ret_piece = piece_msg_paylod.get_u32(); // the zero-based piece index
        let ret_begin = piece_msg_paylod.get_u32(); // the zero-based byte offset within the piece
        let block_data = piece_msg_paylod; // the data for the piece, usually 2^14 bytes long

        anyhow::ensure!(piece as u32 == ret_piece);

        let block = (ret_begin / BLOCK_MAX_SIZE as u32) as usize;
        anyhow::ensure!(block < blocks_count);

        piece_buf
            .get_mut(block)
            .expect("block index was initialized before")
            .put(block_data);

        received_blocks += 1;
        if received_blocks == blocks_count {
            break;
        }
    }

    let piece_data = piece_buf.into_iter().flatten().collect::<Vec<_>>();

    let expected_hash = torrent
        .info
        .hashes
        .get(piece)
        .context("getting hash of a piece")?;

    let computed_hash = sha1::Sha1::digest(&piece_data);

    anyhow::ensure!(
        expected_hash == computed_hash.as_slice(),
        "piece hash of downloaded data does not match the hash from the torrent file"
    );

    std::fs::write(&output, piece_data)
        .with_context(|| format!("writing piece data do file {}", output.as_ref().display()))?;

    Ok(())
}
