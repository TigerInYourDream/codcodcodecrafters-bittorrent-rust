use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use serde_bencode;
use serde_json;
use std::path::PathBuf;
use tracker::{urlencode, TrackerRequest, TrackerResponse};

use crate::torrent::Torrent;

pub mod peers;
pub mod torrent;
pub mod tracker;

pub const BLOCK_MAX: usize = 1 << 14;

#[derive(Debug, Parser)]
pub struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    Decode { value: String },
    Info { torrent: PathBuf },
    Peers { torrent: PathBuf },
}

pub fn decode(encode: &str) -> Result<serde_json::Value> {
    let value = serde_bencode::from_str(encode).map_err(|e| anyhow!(e.to_string()))?;
    convert(value)
}

// serde_bencode::value::Value -> serde_json::Value
pub fn convert(value: serde_bencode::value::Value) -> Result<serde_json::Value> {
    match value {
        serde_bencode::value::Value::Bytes(v) => {
            let string = String::from_utf8(v)?;
            Ok(serde_json::Value::String(string))
        }
        serde_bencode::value::Value::Int(i) => {
            let integers = serde_json::Value::Number(i.into());
            Ok(integers)
        }
        serde_bencode::value::Value::List(list) => {
            let l = list
                .into_iter()
                .map(convert)
                .collect::<Result<Vec<serde_json::Value>>>()?;
            Ok(serde_json::Value::Array(l))
        }
        serde_bencode::value::Value::Dict(d) => {
            let mut map = serde_json::Map::new();
            for (k, v) in d {
                let key = String::from_utf8(k)?;
                let value = convert(v)?;
                map.insert(key, value);
            }
            Ok(serde_json::Value::Object(map))
        }
    }
}

// Usage: your_bittorrent.sh decode "<encoded_value>"
#[tokio::main]
pub async fn main() -> anyhow::Result<()> {
    let arg = Args::parse();
    match arg.command {
        Command::Decode { value } => {
            let decoded_value = decode(&value);
            println!("{:?}", decoded_value);
            match decoded_value {
                Ok(value) => {
                    println!("{}", value.to_string());
                }
                Err(e) => {
                    println!("Error: {}", e);
                }
            }
        }
        Command::Info { torrent } => {
            let file = std::fs::read(torrent)?;
            let t: Torrent = serde_bencode::from_bytes(&file).context("parse torrent file")?;
            println!("Tracker url {:?}", t.announce);
            if let torrent::Keys::SingleFile { length } = t.info.keys {
                println!("File length: {}", length);
            } else {
                todo!("Handle multi-file torrents");
            }
            let hash_info = t.info_hash();
            println!("Info Hash: {}", hex::encode(&hash_info));
            println!("Piece Length: {}", t.info.plength);
            println!("Pieces Hashes:");
            for hash in t.info.pieces.0 {
                print!("{}", hex::encode(hash));
            }
        }
        Command::Peers { torrent } => {
            let dot_torrent = std::fs::read(torrent).context("read torrent file")?;
            let t: Torrent =
                serde_bencode::from_bytes(&dot_torrent).context("parse torrent file")?;
            let length = if let torrent::Keys::SingleFile { length } = t.info.keys {
                length
            } else {
                todo!();
            };

            let info_hash = t.info_hash();
            let request = TrackerRequest {
                peer_id: String::from("00112233445566778899"),
                port: 6881,
                uploaded: 0,
                downloaded: 0,
                left: length,
                compact: 1,
            };

            let url_params =
                serde_urlencoded::to_string(&request).context("url-encode tracker parameters")?;
            let tracker_url = format!(
                "{}?{}&info_hash={}",
                t.announce,
                url_params,
                &urlencode(&info_hash)
            );
            let response = reqwest::get(tracker_url).await.context("query tracker")?;
            let response = response.bytes().await.context("fetch tracker response")?;
            let response: TrackerResponse =
                serde_bencode::from_bytes(&response).context("parse tracker response")?;
            for peer in &response.peers.0 {
                println!("{}:{}", peer.ip(), peer.port());
            }
        }
    }

    Ok(())
}
