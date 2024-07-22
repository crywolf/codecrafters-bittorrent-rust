use std::net::{Ipv4Addr, SocketAddrV4};
use std::path::Path;

use anyhow::Context;
use bytes::Buf;
use serde::{Deserialize, Deserializer, Serialize};

use crate::torrent;

#[derive(Deserialize)]
pub struct TrackerResponse {
    /// An integer, indicating how often the client should make a request to the tracker. Ignored in this challenge.
    pub interval: u64,
    /// A string, which contains list of peers that your client can connect to.
    /// Each peer is represented using 6 bytes. The first 4 bytes are the peer's IP address and the last 2 bytes are the peer's port number.
    pub peers: Peers,
}

pub struct Peers(pub Vec<SocketAddrV4>);

impl Peers {
    pub fn adresses(&self) -> &[SocketAddrV4] {
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
        peer_id: String::from("00112233445566778899"),
        port: 6881,
        uploaded: 0,
        downloaded: 0,
        left: torrent.info.length,
        compact: 1,
    };

    let query = serde_urlencoded::to_string(request).context("serializing request query params")?;
    let info_hash = torrent.info.hash;
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

fn urlencode(t: &[u8; 20]) -> String {
    let mut encoded = String::with_capacity(3 * t.len());
    for &byte in t {
        encoded.push('%');
        encoded.push_str(&hex::encode([byte]));
    }
    encoded
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

        for bytes in value.chunks(6) {
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
