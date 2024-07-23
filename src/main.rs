mod peers;
mod torrent;

use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Decode bencoded value
    Decode { value: String },
    /// Print torrent info
    Info { file: PathBuf },
    /// Print peers to download the file from
    Peers { file: PathBuf },
    /// Establish a TCP connection with a peer and complete a handshak
    Handshake { file: PathBuf, peer_socket: String },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
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
            println!("Info Hash: {}", hex::encode(torrent.info.hash));
            println!("Piece Length: {}", torrent.info.piece_length);
            println!("Info Hashes: ");
            for hash in torrent.info.pieces.chunks_exact(20) {
                println!("{}", hex::encode(hash));
            }
            Ok(())
        }
        Command::Peers { file } => {
            let tracker_response = peers::discover_peers(file)
                .await
                .context("discovering peers")?;
            for peer in tracker_response.peers.adresses() {
                println!("{}", peer);
            }
            Ok(())
        }
        Command::Handshake { file, peer_socket } => {
            // TODO
            // let torrent = torrent::parse_torrent(file).context("parsing torrent file")?;
            // let info_hash = torrent.info.hash;
            // println!("Info Hash: {}", hex::encode(info_hash));
            //println!("peer: {}", peer_socket);

            let remote_peer_id = peers::handshake(file, peer_socket).await?;
            println!("Peer ID: {}", hex::encode(remote_peer_id));
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
