mod codec;
mod download;
pub mod framer;

use std::collections::{BTreeMap, HashSet};
use std::net::{Ipv4Addr, SocketAddrV4};
use std::ops::Deref;
use std::path::Path;
use std::sync::{atomic, Arc};

use anyhow::Context;
use bytes::{Buf, BufMut};
use serde::{Deserialize, Deserializer, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::piece::Piece;
use crate::torrent::Torrent;
use framer::Framer;

const MAX_CONNECTED_PEERS: usize = 5;

#[derive(Deserialize)]
pub struct TrackerResponse {
    /// An integer, indicating how often (in seconds) the client should make a request to the tracker. Ignored in this challenge.
    #[allow(dead_code)]
    pub interval: Option<u64>,
    /// List of peers that the client can connect to.
    pub peers: Peers,
}

/// Deserialized from a string, which contains list of peers that the client can connect to.
/// Each peer is represented using 6 bytes. The first 4 bytes are the peer's IP address and the last 2 bytes are the peer's port number.
#[derive(Clone)]
pub struct Peers(Vec<SocketAddrV4>);

impl Deref for Peers {
    type Target = [SocketAddrV4];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Serialize)]
struct TrackerRequest {
    /// A string of length 20 which this downloader uses as its id
    peer_id: String,
    /// The port number this peer is listening on
    port: u16,
    /// The total amount uploaded so far
    uploaded: usize,
    /// The total amount downloaded so far
    downloaded: usize,
    /// The number of bytes left to download
    left: usize,
    /// Whether the peer list should use the compact representation (https://www.bittorrent.org/beps/bep_0023.html)
    compact: u8,
}

impl TrackerRequest {
    fn new(peer_id: &str, left: usize) -> Self {
        Self {
            peer_id: peer_id.to_string(),
            port: 6881,
            uploaded: 0,
            downloaded: 0,
            left,
            compact: 1,
        }
    }
}

pub struct Downloader {
    /// Peer ID of the downloader
    peer_id: String,
    peers_supporting_extensions: HashSet<[u8; 20]>, // TODO peer id as a type
}

impl Downloader {
    pub fn new() -> Self {
        Self {
            peer_id: Self::random_peer_id(),
            peers_supporting_extensions: HashSet::new(),
        }
    }

    pub fn does_peer_support_extensions(&self, peer_id: &[u8; 20]) -> bool {
        self.peers_supporting_extensions.contains(peer_id)
    }

    pub async fn discover_peers(&self, torrent: &Torrent) -> anyhow::Result<TrackerResponse> {
        let request = TrackerRequest::new(&self.peer_id, torrent.info.length);

        let query =
            serde_urlencoded::to_string(request).context("serializing request query params")?;
        let info_hash = torrent.info.info_hash;
        let url = format!(
            "{}?{}&info_hash={}",
            torrent.announce,
            query,
            Self::urlencode(&info_hash)
        );

        // GET /announce?peer_id=aaaaaaaaaaaaaaaaaaaa&info_hash=aaaaaaaaaaaaaaaaaaaa&port=6881&left=0&downloaded=100&uploaded=0&compact=1
        let response = reqwest::get(url).await.context("querying tracker")?;
        if !response.status().is_success() {
            anyhow::bail!("tracker returned error: {}", response.status());
        }
        let response = response.bytes().await.context("reading bytes response")?;

        let tracker_response =
            serde_bencode::from_bytes(&response).context("deserializing tracker response")?;

        Ok(tracker_response)
    }

    pub async fn handshake(
        &mut self,
        torrent: &Torrent,
        peer_socket: SocketAddrV4,
    ) -> anyhow::Result<([u8; 20], tokio::net::TcpStream)> {
        let mut stream = tokio::net::TcpStream::connect(peer_socket)
            .await
            .context("connecting to peer")?;

        /*
           The handshake is a message consisting of the following parts as described in the peer protocol:

           * length of the protocol string (BitTorrent protocol) which is 19 (1 byte)
           * the string BitTorrent protocol (19 bytes)
           * eight reserved bytes - see below 1)
           * sha1 infohash (20 bytes) (NOT the hexadecimal representation, which is 40 bytes long)
           * peer id (20 bytes). A string of length 20 which this downloader uses as its id.
                Each downloader generates its own id at random at the start of a new download.
                This value will also almost certainly have to be escaped.

            1) Reserved bytes: During the "Peer handshake" stage, the handshake message includes eight reserved bytes (64 bits), all set to zero.
               To signal support for extensions, a client must set the 20th bit from the right (counting starts at 0) in the reserved bytes to 1.
               In Hex, here's how the reserved bytes will look like after setting the 20th bit from the right to 1:

               .... 00010000 00000000 00000000
                       ^ 20th bit from the right, counting starts at 0

               00 00 00 00 00 10 00 00
               (10 in hex is 16 in decimal, which is 00010000 in binary)

               https://www.bittorrent.org/beps/bep_0010.html
        */

        let handshake_len = 1 + 19 + 8 + 20 + 20;
        let mut data = bytes::BytesMut::with_capacity(handshake_len);
        data.put_u8(19);
        data.put_slice(b"BitTorrent protocol");

        if torrent.from_magnet_link {
            // signal support for extensions
            // 00 00 00 00 00 10 00 00
            data.put_bytes(b'\0', 5);
            data.put_u8(16); // == 0x10
            data.put_bytes(b'\0', 2);
        } else {
            // 8 zero bytes
            // 00 00 00 00 00 00 00 00
            data.put_bytes(b'\0', 8);
        }

        data.put_slice(&torrent.info.info_hash);
        data.put_slice(self.peer_id.as_bytes());

        stream
            .write_all(&data)
            .await
            .context("sending handshake data to peer")?;

        let mut resp = Vec::with_capacity(handshake_len);
        resp.reserve_exact(handshake_len);
        resp.put_bytes(b'0', handshake_len);
        stream
            .read_exact(&mut resp)
            .await
            .context("reading handshake response")?;

        anyhow::ensure!(resp.len() == handshake_len);
        anyhow::ensure!(resp[0] == 19);
        anyhow::ensure!(&resp[1..20] == b"BitTorrent protocol");

        let mut remote_peer_id = [0_u8; 20];
        remote_peer_id.copy_from_slice(&resp[handshake_len - 20..]);

        let reserved_bytes = &resp[20..28]; // 8 reserved bytes from response
        if reserved_bytes[5] & 0x10 == 0x10 {
            // peer supports extended messaging
            self.peers_supporting_extensions.insert(remote_peer_id);
        }

        Ok((remote_peer_id, stream))
    }

    pub async fn download_piece(
        &mut self,
        output: impl AsRef<Path>,
        torrent: Torrent,
        piece: usize,
    ) -> anyhow::Result<()> {
        let peers = self
            .discover_peers(&torrent)
            .await
            .context("discovering peers")?
            .peers;

        if peers.is_empty() {
            anyhow::bail!("no peers available")
        }

        let mut stream: Option<tokio::net::TcpStream> = None;
        for &peer in peers.iter() {
            stream = match self.handshake(&torrent, peer).await {
                Ok((_, s)) => Some(s),
                Err(err) => {
                    eprintln!("Performing hanshake with peer {peer} failed with error: {err:?}");
                    None
                }
            };

            if stream.is_some() {
                eprintln!("Succesfull handhake with peer: {peer}");
                break;
            }
        }

        if let Some(stream) = stream {
            let framer = Framer::new(stream, false).await?;
            download::download_piece(framer, output, piece, &torrent).await?;
            Ok(())
        } else {
            anyhow::bail!("failed to connect to any peer")
        }
    }

    pub async fn download_all(
        mut self,
        output: impl AsRef<Path>,
        torrent: Torrent,
    ) -> anyhow::Result<()> {
        let available_peers = self
            .discover_peers(&torrent)
            .await
            .context("discovering peers")?
            .peers;

        if available_peers.is_empty() {
            anyhow::bail!("no peers available")
        }

        let (peer_addrs_tx, mut peer_addrs) =
            tokio::sync::mpsc::channel::<SocketAddrV4>(MAX_CONNECTED_PEERS);
        let (peer_streams_tx, mut peer_streams) =
            tokio::sync::mpsc::channel::<tokio::net::TcpStream>(MAX_CONNECTED_PEERS);
        let (add_peer_tx, mut add_peer_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (finished, mut done) = tokio::sync::mpsc::channel::<()>(1);

        let unconnected_available_peers = available_peers;

        // select peers to connect to
        tokio::spawn(async move {
            let mut attempted_connections = 0;
            let mut unconnected_available_peers_iter = unconnected_available_peers.iter();

            for peer in unconnected_available_peers_iter.by_ref() {
                peer_addrs_tx
                    .send(*peer)
                    .await
                    .expect("peer adresses channel should not be closed");
                attempted_connections += 1;
                if attempted_connections == MAX_CONNECTED_PEERS {
                    break;
                }
            }
            eprintln!("Attempting to connect to {} peers", attempted_connections);

            loop {
                tokio::select! {
                    _ = add_peer_rx.recv() => {
                        eprintln!("-> Request to add new peer");
                        if let Some(&addr) = unconnected_available_peers_iter.next() {
                            eprintln!("-> Adding new peer {}", addr);
                            peer_addrs_tx
                                .send(addr)
                                .await
                                .expect("peer adresses channel should not be closed");
                        } else {
                            eprintln!("-> No available peers left");
                            break;
                        };
                    }
                    _ = done.recv() =>
                    {
                        break;
                    }
                    else => { break; } // both channels closed
                }
            }

            eprintln!("Adding new peers loop finished");
        });

        let cloned_torrent = torrent.clone();

        // connect to peers
        tokio::spawn(async move {
            let mut connected_peers = 0;
            while let Some(peer) = peer_addrs.recv().await {
                //let torrent = torrent_path.clone();
                if let Ok((peer_id, stream)) =
                    self.handshake(&cloned_torrent, peer).await.map_err(|err| {
                        eprintln!(
                            "Performing hanshake with peer {peer} failed with error:\n{err:?}"
                        )
                    })
                {
                    eprintln!("Connected to peer {}", hex::encode(peer_id));
                    connected_peers += 1;
                    peer_streams_tx
                        .send(stream)
                        .await
                        .expect("peer streams channel should not not be closed");
                }
            }
            assert!(connected_peers > 0, "failed to connect to any peer");

            eprintln!("Connecting to {} peers finished", connected_peers);
        });

        let pieces_count = torrent.info.hashes.len();

        // we need MPMC channel for the task queue
        let (needed_pieces, tasks) = kanal::bounded_async::<Piece>(pieces_count);
        let errored_pieces = needed_pieces.clone();

        tokio::spawn(async move {
            // send all pieces to the queue
            for piece_index in 0..pieces_count {
                let tmp_file =
                    tempfile::NamedTempFile::new().expect("creating tmp file should proceed");
                let tmp_file_path = tmp_file.into_temp_path().to_path_buf();
                let piece = Piece {
                    index: piece_index,
                    file_path: tmp_file_path.clone(),
                };

                needed_pieces
                    .send(piece)
                    .await
                    .expect("tasks should not be closed");
            }
            eprintln!("All {} needed pieces sent to the queue", pieces_count);
        });

        let (pieces_tx, mut downloaded_pieces) = tokio::sync::mpsc::channel::<Piece>(pieces_count);

        let downloaded_counter = Arc::new(atomic::AtomicUsize::new(0));

        // every peer connection will take a piece to download from the queue
        eprintln!("Starting download queue");
        while let Some(peer_stream) = peer_streams.recv().await {
            let mut framer = Framer::new(peer_stream, false).await?;

            let tasks = tasks.clone();
            let pieces_tx = pieces_tx.clone();
            let errored_pieces = errored_pieces.clone();
            let add_peer_tx = add_peer_tx.clone();
            let finished = finished.clone();
            let torrent = torrent.clone();

            let downloaded_counter = Arc::clone(&downloaded_counter);
            tokio::spawn(async move {
                while let Ok(piece) = tasks.recv().await {
                    let piece_index = piece.index;
                    eprintln!("---> downloading piece {}", piece_index);
                    match download::download_piece(
                        framer,
                        piece.file_path.clone(),
                        piece.index,
                        &torrent,
                    )
                    .await
                    {
                        Ok(f) => {
                            framer = f;
                            pieces_tx
                                .send(piece)
                                .await
                                .expect("pieces receiver should not be closed");

                            downloaded_counter.fetch_add(1, atomic::Ordering::AcqRel);
                        }

                        Err(err) => {
                            // return failed piece back to the queue
                            eprintln!("{:?}, returning piece {} to the queue", err, piece_index);
                            errored_pieces
                                .send(piece)
                                .await
                                .expect("needed pieces queue shoud not be closed");

                            // connect to some new peer
                            if !add_peer_tx.is_closed() {
                                add_peer_tx
                                    .send(())
                                    .await
                                    .expect("add_peer channel is not closed");
                            }
                            // TODO Reconnect to the failed peer? (ie. add it to the list of available peers again?)

                            break;
                        }
                    }
                    eprintln!("<--- piece {} downloaded", piece_index);

                    if downloaded_counter.load(atomic::Ordering::Acquire) == pieces_count {
                        eprintln!(
                            "All {} pieces downloaded, closing download queue",
                            pieces_count
                        );
                        if !finished.is_closed() {
                            finished
                                .send(())
                                .await
                                .expect("finished channel is not closed");
                            break;
                        }
                    }
                }
            });
        }

        let mut piece_files = BTreeMap::new();

        while let Some(piece) = downloaded_pieces.recv().await {
            eprintln!("<<< received piece {}", piece.index);
            piece_files.insert(piece.index, piece.file_path);
            if piece_files.len() == pieces_count {
                break;
            }
        }

        let mut output_file = tokio::fs::File::create(&output).await?;

        for (_, file_name) in piece_files {
            let mut piece_file = tokio::fs::File::open(file_name).await?;
            tokio::io::copy(&mut piece_file, &mut output_file)
                .await
                .with_context(|| {
                    format!("writing piece data to file {}", output.as_ref().display())
                })?;
        }

        Ok(())
    }

    fn urlencode(t: &[u8; 20]) -> String {
        let mut encoded = String::with_capacity(3 * t.len());
        for &byte in t {
            encoded.push('%');
            encoded.push_str(&hex::encode([byte]));
        }
        encoded
    }

    /// A string of length 20 which this downloader uses as its id.
    /// Each downloader generates its own id at random at the start of a new download.
    /// This value will almost certainly have to be escaped.
    fn random_peer_id() -> String {
        std::iter::repeat_with(fastrand::alphanumeric)
            .take(20)
            .collect()
    }
}

use std::fmt;

use serde::de::{self, Visitor};

struct PeersVisitor;

impl<'de> Visitor<'de> for PeersVisitor {
    type Value = Peers;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("6 bytes. The first 4 bytes are the peer's IP address and the last 2 bytes are the peer's port number.")
    }

    fn visit_bytes<E>(self, value: &[u8]) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        if value.len() % 6 != 0 {
            return Err(E::custom(format!("expecting 6 bytes, got {}", value.len())));
        }

        let mut socket_addrs = Vec::new();

        for bytes in value.chunks_exact(6) {
            let ip = &bytes[..4];
            let mut port = &bytes[4..];
            let ip_addr = Ipv4Addr::new(ip[0], ip[1], ip[2], ip[3]);
            let socket = SocketAddrV4::new(ip_addr, port.get_u16());
            socket_addrs.push(socket)
        }
        Ok(Peers(socket_addrs))
    }
}

impl<'de> Deserialize<'de> for Peers {
    fn deserialize<D>(deserializer: D) -> Result<Peers, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_seq(PeersVisitor)
    }
}
