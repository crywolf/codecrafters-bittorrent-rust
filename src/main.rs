use std::env;

// Available if you need it!
// use serde_bencode

fn decode_bencoded_value(encoded_value: &str) -> (serde_json::Value, &str) {
    if let Some('0'..='9') = encoded_value.chars().next() {
        if let Some((len, rest)) = encoded_value.split_once(':') {
            if let Ok(len) = len.parse::<usize>() {
                return (rest[..len].to_string().into(), &rest[len..]);
            }
        }
    }

    panic!("Unhandled encoded value: {}", encoded_value);
}

// Usage: your_bittorrent.sh decode "<encoded_value>"
fn main() {
    let args: Vec<String> = env::args().collect();
    let command = &args[1];

    if command == "decode" {
        let encoded_value = &args[2];
        let decoded_value = decode_bencoded_value(encoded_value).0;
        println!("{}", decoded_value);
    } else {
        println!("unknown command: {}", args[1])
    }
}
