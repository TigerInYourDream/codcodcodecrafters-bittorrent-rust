use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use futures_util::{SinkExt, StreamExt};
use peer::Handshake;
use serde_bencode;
use serde_json;
use sha1::{Digest, Sha1};
use std::{net::SocketAddrV4, path::PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracker::{urlencode, TrackerRequest, TrackerResponse};

use crate::{
    peer::{Message, MessageFramer, MessageTag, Piece, Request},
    torrent::Torrent,
};

pub mod peer;
pub mod torrent;
pub mod tracker;
pub mod download;
pub mod piece;

pub const BLOCK_MAX: usize = 1 << 14;

#[derive(Debug, Parser)]
pub struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
// snake case for subcommands
#[clap(rename_all = "snake_case")]
pub enum Command {
    Decode {
        value: String,
    },
    Info {
        torrent: PathBuf,
    },
    Peers {
        torrent: PathBuf,
    },
    Handshake {
        torrent: PathBuf,
        peer: String,
    },
    DownloadPiece {
        #[arg(short)]
        output: PathBuf,
        torrent: PathBuf,
        piece: usize,
    },
    Download {
        #[arg(short)]
        output: PathBuf,
        torrent: PathBuf,
    }
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
        Command::Handshake { torrent, peer } => {
            let dot_torrent = std::fs::read(torrent).context("read torrent file")?;
            let t: Torrent =
                serde_bencode::from_bytes(&dot_torrent).context("parse torrent file")?;

            let info_hash = t.info_hash();
            let peer = peer.parse::<SocketAddrV4>().context("parse peer address")?;
            let mut peer = tokio::net::TcpStream::connect(peer)
                .await
                .context("connect to peer")?;
            let mut handshake = Handshake::new(info_hash, *b"00112233445566778899");
            {
                // copy from joohoo's code
                let handshake_bytes =
                    &mut handshake as *mut Handshake as *mut [u8; std::mem::size_of::<Handshake>()];
                // Safety: Handshake is a POD with repr(c) and repr(packed)
                let handshake_bytes: &mut [u8; std::mem::size_of::<Handshake>()] =
                    unsafe { &mut *handshake_bytes };
                peer.write_all(handshake_bytes)
                    .await
                    .context("write handshake")?;
                peer.read_exact(handshake_bytes)
                    .await
                    .context("read handshake")?;
            }
            assert_eq!(handshake.length, 19);
            assert_eq!(&handshake.bittorrent, b"BitTorrent protocol");
            println!("Peer ID: {}", hex::encode(&handshake.peer_id));
        }
        Command::DownloadPiece {
            output,
            torrent,
            piece: piece_i,
        } => {
            // comples code
            let dot_torrent = std::fs::read(torrent).context("read torrent file")?;
            let t: Torrent =
                serde_bencode::from_bytes(&dot_torrent).context("parse torrent file")?;
            let length = if let torrent::Keys::SingleFile { length } = t.info.keys {
                length
            } else {
                todo!();
            };
            assert!(piece_i < t.info.pieces.0.len());

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
            let tracker_info: TrackerResponse =
                serde_bencode::from_bytes(&response).context("parse tracker response")?;

            let peer = &tracker_info.peers.0[0];
            let mut peer = tokio::net::TcpStream::connect(peer)
                .await
                .context("connect to peer")?;
            let mut handshake = Handshake::new(info_hash, *b"00112233445566778899");
            {
                let handshake_bytes = handshake.as_bytes_mut();
                peer.write_all(handshake_bytes)
                    .await
                    .context("write handshake")?;
                peer.read_exact(handshake_bytes)
                    .await
                    .context("read handshake")?;
            }
            assert_eq!(handshake.length, 19);
            assert_eq!(&handshake.bittorrent, b"BitTorrent protocol");

            let mut peer = tokio_util::codec::Framed::new(peer, MessageFramer);
            let bitfield = peer
                .next()
                .await
                .expect("peer always sends a bitfields")
                .context("peer message was invalid")?;
            assert_eq!(bitfield.tag, MessageTag::Bitfield);
            // NOTE: we assume that the bitfield covers all pieces

            peer.send(Message {
                tag: MessageTag::Interested,
                payload: Vec::new(),
            })
            .await
            .context("send interested message")?;

            let unchoke = peer
                .next()
                .await
                .expect("peer always sends an unchoke")
                .context("peer message was invalid")?;
            assert_eq!(unchoke.tag, MessageTag::Unchoke);
            assert!(unchoke.payload.is_empty());

            let piece_hash = &t.info.pieces.0[piece_i];
            let piece_size = if piece_i == t.info.pieces.0.len() - 1 {
                let md = length % t.info.plength;
                if md == 0 {
                    t.info.plength
                } else {
                    md
                }
            } else {
                t.info.plength
            };
            // the + (BLOCK_MAX - 1) rounds up
            let nblocks = (piece_size + (BLOCK_MAX - 1)) / BLOCK_MAX;
            let mut all_blocks = Vec::with_capacity(piece_size);
            for block in 0..nblocks {
                let block_size = if block == nblocks - 1 {
                    let md = piece_size % BLOCK_MAX;
                    if md == 0 {
                        BLOCK_MAX
                    } else {
                        md
                    }
                } else {
                    BLOCK_MAX
                };
                let mut request = Request::new(
                    piece_i as u32,
                    (block * BLOCK_MAX) as u32,
                    block_size as u32,
                );
                let request_bytes = Vec::from(request.as_bytes_mut());
                peer.send(Message {
                    tag: MessageTag::Request,
                    payload: request_bytes,
                })
                .await
                .with_context(|| format!("send request for block {block}"))?;

                let piece = peer
                    .next()
                    .await
                    .expect("peer always sends a piece")
                    .context("peer message was invalid")?;
                assert_eq!(piece.tag, MessageTag::Piece);
                assert!(!piece.payload.is_empty());

                let piece = Piece::ref_from_bytes(&piece.payload[..])
                    .expect("always get all Piece response fields from peer");
                assert_eq!(piece.index() as usize, piece_i);
                assert_eq!(piece.begin() as usize, block * BLOCK_MAX);
                assert_eq!(piece.block().len(), block_size);
                all_blocks.extend(piece.block());
            }
            assert_eq!(all_blocks.len(), piece_size);

            let mut hasher = Sha1::new();
            hasher.update(&all_blocks);
            let hash: [u8; 20] = hasher
                .finalize()
                .try_into()
                .expect("GenericArray<_, 20> == [_; 20]");
            assert_eq!(&hash, piece_hash);

            tokio::fs::write(&output, all_blocks)
                .await
                .context("write out downloaded piece")?;
            println!("Piece {piece_i} downloaded to {}.", output.display());
        }
        Command::Download { output, torrent } => {
            let torrent = Torrent::read(torrent).await?;
            torrent.print_tree();
            // torrent.download_all_to_file(output).await?;
            let files = torrent.download_all().await?;
            tokio::fs::write(
                output,
                files.into_iter().next().expect("always one file").bytes(),
            )
            .await?;            

        }
    }

    Ok(())
}
