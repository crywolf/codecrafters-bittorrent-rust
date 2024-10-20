use anyhow::{anyhow, Context};
use serde::{Deserialize, Serialize};
use sha1::Digest;
use std::{
    borrow::Cow,
    fs,
    ops::{Deref, DerefMut},
    path::PathBuf,
};

/// Metainfo files (also known as .torrent files) are bencoded dictionaries
/// https://www.bittorrent.org/beps/bep_0003.html#metainfo-files
#[derive(Clone, Deserialize, Debug)]
pub struct Torrent {
    /// The URL of the tracker, which is a central server
    /// that keeps track of peers participating in the sharing of a torrent
    pub announce: String,

    /// Info dictionary with keys described below
    pub info: Info,

    /// Torrent was obtainded from magnet link
    #[serde(skip)]
    pub is_from_magnet_link: bool,
}

#[derive(Clone, Deserialize, Serialize, Debug)]
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
    pub info_hash: Hash,
}

impl Torrent {
    pub fn from_magnet_info(magnet: MagnetInfo) -> Self {
        let mut info = Info::new();
        info.info_hash = magnet.info_hash;
        info.length = 999; // workaround: cannot be zero, but we do not know the file length
        info.name = magnet.name.unwrap_or_default();

        Self {
            announce: magnet.announce.unwrap(),
            info,
            is_from_magnet_link: true,
        }
    }
}

impl Info {
    pub fn new() -> Self {
        Self {
            length: 0,
            name: String::new(),
            piece_length: 0,
            pieces: Vec::new(),
            hashes: Hashes::default(),
            info_hash: Hash::default(),
        }
    }

    /// Computes info_hash and populates the list of hashes of the pieces
    pub fn finalize(&mut self) -> anyhow::Result<()> {
        let info_bencoded = serde_bencode::to_bytes(&self).context("bencoding Info dictionary")?;

        let digest = sha1::Sha1::digest(info_bencoded);
        *self.info_hash = digest.into();

        if self.pieces.len() % 20 != 0 {
            anyhow::bail!(
                "pieces' lenght must be multiple of 20, but is {}",
                self.pieces.len()
            );
        }
        let hashes = Hashes(
            self.pieces
                .chunks_exact(20)
                .map(|slice_20| slice_20.try_into().expect("guaranteed to be length 20"))
                .collect(),
        );

        self.hashes = hashes;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct Hash([u8; 20]);

impl Hash {
    pub fn new() -> Self {
        Hash::default()
    }

    pub fn hex(&self) -> String {
        hex::encode(self)
    }

    pub fn urlencode(&self) -> String {
        let mut encoded = String::with_capacity(3 * self.0.len());
        for byte in self.0 {
            encoded.push('%');
            encoded.push_str(&hex::encode([byte]));
        }
        encoded
    }
}

impl Deref for Hash {
    type Target = [u8; 20];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for Hash {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl AsRef<[u8]> for Hash {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl TryFrom<&[u8]> for Hash {
    type Error = anyhow::Error;

    fn try_from(value: &[u8]) -> Result<Self, Self::Error> {
        Ok(Hash(value.try_into()?))
    }
}

#[derive(Clone, Default, Debug)]
pub struct Hashes(Vec<Hash>);

impl Deref for Hashes {
    type Target = Vec<Hash>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

pub fn parse_torrent(file: PathBuf) -> anyhow::Result<Torrent> {
    let bytes = fs::read(file).context("reading torrent file")?;

    let mut torrent: Torrent =
        serde_bencode::from_bytes(&bytes).context("deserializing torrent data")?;

    torrent.info.finalize().context("finalize torrent info")?;

    Ok(torrent)
}

/// Magnet links allow users to download files from peers without needing a torrent file.
/// https://www.bittorrent.org/beps/bep_0009.html
/// https://en.wikipedia.org/wiki/Magnet_URI_scheme
pub struct MagnetInfo {
    /// SHA-1 hash of bencoded `Info` dictionary (required)
    pub info_hash: Hash,
    /// The URL of the tracker. A magnet link can contain multiple tracker URLs, but for the purposes of this challenge it'll only contain one.
    pub announce: Option<String>,
    /// Suggested name to save the file / directory as
    pub name: Option<String>,
}

pub fn parse_magnet_link(magnet_link: &str) -> anyhow::Result<MagnetInfo> {
    let mut announce = None;
    let mut name = None;
    let mut info_hash = Hash::new();

    let url = reqwest::Url::parse(magnet_link)?;
    for (k, v) in url.query_pairs() {
        match k {
            Cow::Borrowed("tr") => announce = Some(v.into_owned()),
            Cow::Borrowed("dn") => name = Some(v.into_owned()),
            Cow::Borrowed("xt") => {
                let ih = v.into_owned();
                let ih = ih
                    .strip_prefix("urn:btih:")
                    .ok_or(anyhow!("malformed magnet link").context("parsing xt param"))?;
                hex::decode_to_slice(ih, info_hash.as_mut())
                    .context("decode xt from hex to bytes")?;
            }
            _ => anyhow::bail!("invalid magnet link: unknown '{}' param", k),
        }
    }

    let magnet = MagnetInfo {
        announce,
        info_hash,
        name,
    };

    Ok(magnet)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_magnet_link() {
        let ml = "magnet:?xt=urn:btih:d69f91e6b2ae4c542468d1073a71d4ea13879a7f&dn=sample.torrent&tr=http%3A%2F%2Fbittorrent-test-tracker.codecrafters.io%2Fannounce";
        let r = parse_magnet_link(ml).unwrap();

        assert_eq!(
            r.announce,
            Some("http://bittorrent-test-tracker.codecrafters.io/announce".to_string())
        );
        assert_eq!(r.name, Some("sample.torrent".to_string()));
        assert_eq!(
            r.info_hash.hex(),
            "d69f91e6b2ae4c542468d1073a71d4ea13879a7f"
        );
    }
}
