use anyhow::Context;
use serde::{Deserialize, Serialize};
use sha1::Digest;
use std::{fs, ops::Deref, path::PathBuf};

/// Metainfo files (also known as .torrent files) are bencoded dictionaries
/// https://www.bittorrent.org/beps/bep_0003.html#metainfo-files
#[derive(Deserialize)]
pub struct Torrent {
    /// The URL of the tracker, which is a central server
    /// that keeps track of peers participating in the sharing of a torrent
    pub announce: String,

    /// Info dictionary with keys described below
    pub info: Info,
}

#[derive(Deserialize, Serialize)]
pub struct Info {
    /// size of the file in bytes, for single-file torrents
    pub length: usize,

    /// suggested name to save the file / directory as
    pub name: String,

    /// 'piece length' number of bytes in each piece
    ///
    /// 'piece length' maps to the number of bytes in each piece the file is split into.
    /// For the purposes of transfer, files are split into fixed-size pieces which are all the same length
    /// except for possibly the last one which may be truncated.
    /// 'piece length' is almost always a power of two, most commonly 2^18 = 256K (BitTorrent prior to version 3.2 uses 2^20 = 1M as default).
    #[serde(rename = "piece length")]
    pub piece_length: usize,

    // Concatenated SHA-1 hashes of each piece
    #[serde(with = "serde_bytes")]
    pieces: Vec<u8>,

    /// Each entry of `pieces` is the SHA-1 hash of the piece at the corresponding index.
    #[serde(skip)]
    pub hashes: Hashes,

    /// SHA-1 hash of this bencoded Info dictionary
    #[serde(skip)]
    pub info_hash: [u8; 20],
}

#[derive(Default)]
pub struct Hashes(Vec<[u8; 20]>);

impl Deref for Hashes {
    type Target = Vec<[u8; 20]>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

pub fn parse_torrent(file: PathBuf) -> anyhow::Result<Torrent> {
    let bytes = fs::read(file).context("reading torrent file")?;

    let mut torrent: Torrent =
        serde_bencode::from_bytes(&bytes).context("deserializing torrent data")?;

    let info_bencoded =
        serde_bencode::to_bytes(&torrent.info).context("bencoding Info dictionary")?;

    let digest = sha1::Sha1::digest(info_bencoded);

    if torrent.info.pieces.len() % 20 != 0 {
        anyhow::bail!(
            "pieces' lenght must be multiple of 20, but is {}",
            torrent.info.pieces.len()
        );
    }
    let hashes = Hashes(
        torrent
            .info
            .pieces
            .chunks_exact(20)
            .map(|slice_20| slice_20.try_into().expect("guaranteed to be length 20"))
            .collect(),
    );

    torrent.info.hashes = hashes;
    torrent.info.info_hash = digest.into();

    Ok(torrent)
}
