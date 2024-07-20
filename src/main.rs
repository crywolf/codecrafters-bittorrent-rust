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
}

fn main() {
    let args = Cli::parse();
    match args.command {
        Command::Decode { value } => {
            let (decoded_value, _) = decode_bencoded_value(&value);
            println!("{}", decoded_value);
        }
    }
}

fn decode_bencoded_value(encoded_value: &str) -> (serde_json::Value, &str) {
    match encoded_value.chars().next() {
        Some('0'..='9') => {
            // Example: "5:hello" -> "hello"
            if let Some((len, rest)) = encoded_value.split_once(':') {
                if let Ok(len) = len.parse::<usize>() {
                    return (rest[..len].to_string().into(), &rest[len..]);
                }
            }
        }
        Some('i') => {
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
        _ => {}
    }

    panic!("Unhandled encoded value: {}", encoded_value);
}
