mod codec;
mod download;

use std::net::{Ipv4Addr, SocketAddrV4};
use std::ops::Deref;
use std::path::Path;

use anyhow::Context;
use bytes::{Buf, BufMut};

use serde::{Deserialize, Deserializer, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::torrent;

const PEER_ID: &str = "00112233445566778899";

#[derive(Deserialize)]
pub struct TrackerResponse {
    /// An integer, indicating how often (in seconds) the client should make a request to the tracker. Ignored in this challenge.
    pub interval: u64,
    /// List of peers that the client can connect to.
    pub peers: Peers,
}

/// Deserialized from a string, which contains list of peers that the client can connect to.
/// Each peer is represented using 6 bytes. The first 4 bytes are the peer's IP address and the last 2 bytes are the peer's port number.
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

pub async fn discover_peers(file: impl AsRef<Path>) -> anyhow::Result<TrackerResponse> {
    let torrent = torrent::parse_torrent(file.as_ref().into()).context("parsing torrent file")?;

    let request = TrackerRequest {
        peer_id: String::from(PEER_ID),
        port: 6881,
        uploaded: 0,
        downloaded: 0,
        left: torrent.info.length,
        compact: 1,
    };

    let query = serde_urlencoded::to_string(request).context("serializing request query params")?;
    let info_hash = torrent.info.info_hash;
    let url = format!(
        "{}?{}&info_hash={}",
        torrent.announce,
        query,
        urlencode(&info_hash)
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
    file: impl AsRef<Path>,
    peer_socket: String,
) -> anyhow::Result<([u8; 20], tokio::net::TcpStream)> {
    let torrent = torrent::parse_torrent(file.as_ref().into()).context("parsing torrent file")?;

    let peer_socket = peer_socket
        .parse::<SocketAddrV4>()
        .context("parsing peer address")?;

    let mut stream = tokio::net::TcpStream::connect(peer_socket)
        .await
        .context("connecting to peer")?;

    /*
       The handshake is a message consisting of the following parts as described in the peer protocol:

       * length of the protocol string (BitTorrent protocol) which is 19 (1 byte)
       * the string BitTorrent protocol (19 bytes)
       * eight reserved bytes, which are all set to zero (8 bytes)
       * sha1 infohash (20 bytes) (NOT the hexadecimal representation, which is 40 bytes long)
       * peer id (20 bytes) (you can use 00112233445566778899 for this challenge)
    */
    let handshake_len = 1 + 19 + 8 + 20 + 20;
    let mut data = bytes::BytesMut::with_capacity(handshake_len);
    data.put_u8(19);
    data.put_slice(b"BitTorrent protocol");
    data.put_bytes(b'0', 8);
    data.put_slice(&torrent.info.info_hash);
    data.put_slice(PEER_ID.as_bytes());

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

    Ok((remote_peer_id, stream))
}

fn urlencode(t: &[u8; 20]) -> String {
    let mut encoded = String::with_capacity(3 * t.len());
    for &byte in t {
        encoded.push('%');
        encoded.push_str(&hex::encode([byte]));
    }
    encoded
}

pub async fn download_piece(
    output: impl AsRef<Path>,
    torrent: impl AsRef<Path>,
    piece: usize,
) -> anyhow::Result<()> {
    let peers = discover_peers(torrent.as_ref())
        .await
        .context("discovering peers")?
        .peers;

    if peers.is_empty() {
        anyhow::bail!("no peers available")
    }

    let mut stream: Option<tokio::net::TcpStream> = None;
    for &peer in peers.iter() {
        stream = match handshake(torrent.as_ref(), peer.to_string()).await {
            Ok((_, s)) => Some(s),
            Err(err) => {
                eprintln!("Rerforming hanshake with peer {peer} failed with error: {err:?}");
                None
            }
        };

        if stream.is_some() {
            eprintln!("succesfull handhake with peer: {peer}");
            break;
        }
    }

    if let Some(stream) = stream {
        let torrent =
            torrent::parse_torrent(torrent.as_ref().into()).context("parsing torrent file")?;

        download::download_piece(stream, output, piece, &torrent).await
    } else {
        anyhow::bail!("failed to connect to any peer")
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
