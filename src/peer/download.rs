use std::path::Path;

use anyhow::Context;
use bytes::{Buf, BufMut};
use futures_util::{SinkExt, StreamExt};
use sha1::Digest;

use super::framer::{Framer, Message, MessageTag};
use crate::torrent::Torrent;

const BLOCK_MAX_SIZE: usize = 1 << 14; // 2^14 (16 kiB)

// Requests are pipelined to improve download speeds.
// BitTorrent Economics Paper [http://bittorrent.org/bittorrentecon.pdf] recommends having 5 requests pending at once,
// to avoid a delay between blocks being sent.
const PIPELINED_REQUESTS: usize = 5;

pub async fn download_piece(
    mut framer: Framer,
    output: impl AsRef<Path>,
    piece: usize,
    torrent: &Torrent,
) -> anyhow::Result<Framer> {
    let pieces_count = torrent.info.hashes.len();

    if piece > pieces_count {
        anyhow::bail!("piece argument out of range, max value is {}", pieces_count)
    }

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
    let (framer_tx, framer_rx) = tokio::sync::oneshot::channel::<Framer>();

    tokio::spawn(async move {
        let mut received_blocks = 0;
        loop {
            tokio::select! {
                // 4. Send a Request message for each block
                Some(message) = request_rx.recv() => {
                    _ = framer
                        .send(message)
                        .await
                        .context("sending Request message").map_err(|e| eprintln!("Error: {:?}", e));
                }

                // 5. Wait for a Piece message for each block you've requested
                Some(resp) = framer.next() => {
                    if let Ok(message) = resp {
                        if message.tag == MessageTag::Piece { // ignoring other message types
                            response_tx
                            .send(message)
                            .await
                            .expect("receiver should not be closed");

                            received_blocks += 1;

                            if received_blocks == blocks_count { break; }
                        }
                    } else {
                        eprintln!("Error: {:?}", resp.context("reading Piece message"));
                        break;
                    }
                }

                else => { break; }
            }
        }

        // ending request/response loop, returning framer
        framer_tx
            .send(framer)
            .expect("receiver should not be closed");
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

    tokio::fs::write(&output, piece_data)
        .await
        .with_context(|| format!("writing piece data to file {}", output.as_ref().display()))?;

    let framer = framer_rx.await?;

    Ok(framer)
}
