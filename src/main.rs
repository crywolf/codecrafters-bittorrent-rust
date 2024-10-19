mod peer;
mod piece;
mod torrent;

use std::{net::SocketAddrV4, path::PathBuf};

use crate::peer::framer::Framer;
use anyhow::{anyhow, Context};
use clap::{Parser, Subcommand};
use torrent::Torrent;

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
#[clap(rename_all = "snake_case")]
enum Command {
    /// Decode bencoded value
    Decode { value: String },
    /// Print torrent info
    Info { file: PathBuf },
    /// Print peers to download the file from
    Peers { file: PathBuf },
    /// Establish a TCP connection with a peer and complete a handshake
    Handshake { file: PathBuf, peer_socket: String },
    /// Download one piece and save it to disk
    DownloadPiece {
        /// Output file
        #[arg(short)]
        output: PathBuf,
        /// Torrent file
        torrent: PathBuf,
        /// Piece number
        piece: usize,
    },
    /// Download the whole file and save it to disk
    Download {
        /// Output file
        #[arg(short)]
        output: PathBuf,
        /// Torrent file
        torrent: PathBuf,
    },
    /// Parse magnet link
    MagnetParse { magnet_link: String },
    /// Establish a TCP connection with a peer and complete a handshake using a magnet link
    MagnetHandshake { magnet_link: String },
    /// Request torrent metadada using magnet link
    MagnetInfo { magnet_link: String },
    MagnetDownloadPiece {
        /// Output file
        #[arg(short)]
        output: PathBuf,
        /// Magnet link
        magnet_link: String,
        /// Piece number
        piece: usize,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut downloader = peer::Downloader::new();

    let args = Cli::parse();
    match args.command {
        Command::Decode { value } => {
            let (decoded_value, _) = decode_bencoded_value(&value);
            println!("{}", decoded_value);
            Ok(())
        }
        Command::Info { file } => {
            let torrent = torrent::parse_torrent(file).context("parsing torrent file")?;
            println!("Tracker URL: {}", torrent.announce);
            println!("Length: {}", torrent.info.length);
            println!("Info Hash: {}", torrent.info.info_hash.hex());
            println!("Piece Length: {}", torrent.info.piece_length);
            println!("Info Hashes: ");
            for hash in torrent.info.hashes.iter() {
                println!("{}", hash.hex());
            }
            Ok(())
        }
        Command::Peers { file } => {
            let torrent = torrent::parse_torrent(file).context("parsing torrent file")?;
            let tracker_response = downloader
                .discover_peers(&torrent)
                .await
                .context("discovering peers")?;
            for &peer in tracker_response.peers.iter() {
                println!("{}", peer);
            }
            Ok(())
        }
        Command::Handshake { file, peer_socket } => {
            let torrent = torrent::parse_torrent(file).context("parsing torrent file")?;
            let peer_socket = peer_socket
                .parse::<SocketAddrV4>()
                .context("parsing peer address")?;

            let (remote_peer_id, _) = downloader.handshake(&torrent, peer_socket).await?;
            println!("Peer ID: {}", remote_peer_id.hex());
            Ok(())
        }
        Command::DownloadPiece {
            output,
            torrent,
            piece,
        } => {
            let torrent = torrent::parse_torrent(torrent).context("parsing torrent file")?;

            downloader.download_piece(&output, torrent, piece).await?;
            println!("Piece {} downloaded to {}.", piece, output.display());
            Ok(())
        }
        Command::Download { output, torrent } => {
            let torrent_file = torrent;
            let torrent =
                torrent::parse_torrent(torrent_file.clone()).context("parsing torrent file")?;

            downloader.download_all(&output, torrent).await?;
            println!(
                "Downloaded {} to {}.",
                torrent_file.display(),
                output.display()
            );
            Ok(())
        }
        Command::MagnetParse { magnet_link } => {
            let magnet = torrent::parse_magnet_link(&magnet_link).context("parsing magnet link")?;
            println!(
                "Tracker URL: {}",
                magnet
                    .announce
                    .ok_or(anyhow!("Missing tracker URL in magnet link"))?
            );
            println!("Info Hash: {}", magnet.info_hash.hex());
            Ok(())
        }
        Command::MagnetHandshake { magnet_link } => {
            let magnet = torrent::parse_magnet_link(&magnet_link).context("parsing magnet link")?;
            let torrent = Torrent::from_magnet_info(magnet);

            let tracker_response = downloader
                .discover_peers(&torrent)
                .await
                .context("discovering peers")?;
            let &peer_socket = tracker_response
                .peers
                .first()
                .ok_or(anyhow!("Could not find any peer"))?;

            let (remote_peer_id, stream) = downloader.handshake(&torrent, peer_socket).await?;

            let ext_support = downloader.does_peer_support_extensions(&remote_peer_id);
            let framer = Framer::new(stream, ext_support).await?;

            println!("Peer ID: {}", remote_peer_id.hex());
            if let Some(peer_extension_id) = framer.peer_metadata_extension_id() {
                println!("Peer Metadata Extension ID: {}", peer_extension_id);
            }

            Ok(())
        }
        Command::MagnetInfo { magnet_link } => {
            let magnet = torrent::parse_magnet_link(&magnet_link).context("parsing magnet link")?;
            let magnet_info_hash = magnet.info_hash;

            let mut torrent = Torrent::from_magnet_info(magnet);

            let tracker_response = downloader
                .discover_peers(&torrent)
                .await
                .context("discovering peers")?;
            let &peer_socket = tracker_response
                .peers
                .first()
                .ok_or(anyhow!("Could not find any peer"))?;

            let (remote_peer_id, stream) = downloader.handshake(&torrent, peer_socket).await?;

            let ext_support = downloader.does_peer_support_extensions(&remote_peer_id);
            let mut framer = Framer::new(stream, ext_support).await?;

            let magnet_torrent_info = framer
                .magnet_torrent_info()
                .await
                .context("get magnet torrent info")?
                .ok_or(anyhow::anyhow!(
                    "magnet torrent info is missing, does peer supoort magnet extension?"
                ))?;

            torrent.info = magnet_torrent_info;

            println!("Tracker URL: {}", torrent.announce);
            println!("Length: {}", torrent.info.length);
            println!("Info Hash: {}", torrent.info.info_hash.hex());
            println!("Piece Length: {}", torrent.info.piece_length);
            println!("Info Hashes: ");
            for hash in torrent.info.hashes.iter() {
                println!("{}", hash.hex());
            }

            anyhow::ensure!(
                magnet_info_hash == torrent.info.info_hash,
                "Info Hash mismatch: expected {}, got {}",
                magnet_info_hash.hex(),
                torrent.info.info_hash.hex()
            );

            Ok(())
        }
        Command::MagnetDownloadPiece {
            output,
            magnet_link,
            piece,
        } => {
            let magnet = torrent::parse_magnet_link(&magnet_link).context("parsing magnet link")?;
            let torrent = Torrent::from_magnet_info(magnet);

            downloader.download_piece(&output, torrent, piece).await?;
            println!("Piece {} downloaded to {}.", piece, output.display());

            Ok(())
        }
    }
}

fn decode_bencoded_value(encoded_value: &str) -> (serde_json::Value, &str) {
    match encoded_value.chars().next() {
        Some('0'..='9') => {
            // <length>:<contents>
            // Example: "5:hello" -> "hello"
            if let Some((len, rest)) = encoded_value.split_once(':') {
                if let Ok(len) = len.parse::<usize>() {
                    return (rest[..len].to_string().into(), &rest[len..]);
                }
            }
        }
        Some('i') => {
            // i<number>e
            // Example: "i-42e" -> -42
            if let Some((n, rest)) =
                encoded_value
                    .split_at(1)
                    .1
                    .split_once('e')
                    .and_then(|(n, rest)| {
                        let n = n.parse::<i64>().ok()?;
                        Some((n, rest))
                    })
            {
                return (n.into(), rest);
            }
        }
        Some('l') => {
            // l<bencoded_elements>e
            // Example: l5:helloi52ee -> ["hello", 52] ; lli4eei5ee -> [[4],5]
            let mut rest = encoded_value.split_at(1).1;
            let mut list = Vec::new();
            while !rest.starts_with('e') && !rest.is_empty() {
                let (value, remainder) = decode_bencoded_value(rest);
                list.push(value);
                rest = remainder;
            }
            return (list.into(), &rest[1..]);
        }
        Some('d') => {
            // d<key1><value1>...<keyN><valueN>e. The keys are sorted in lexicographical order and must be strings.
            // Example: d3:foo3:bar5:helloi52ee -> {"hello": 52, "foo":"bar"}
            let mut rest = encoded_value.split_at(1).1;
            let mut dictionary = serde_json::Map::new();
            while !rest.starts_with('e') && !rest.is_empty() {
                let (key, remainder) = decode_bencoded_value(rest);
                let (val, remainder) = decode_bencoded_value(remainder);
                let key = match key {
                    serde_json::Value::String(key) => key,
                    key => panic!("Key must be a string, but is {:?}", key),
                };
                dictionary.insert(key, val);
                rest = remainder;
            }
            return (dictionary.into(), &rest[1..]);
        }
        _ => {}
    }

    panic!("Unhandled encoded value: {}", encoded_value);
}
