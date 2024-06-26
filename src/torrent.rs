use std::path::Path;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use sha1::Digest;

use crate::download::{self, Downloaded};

use self::hashes::Hashes;

#[derive(Deserialize, Serialize, Debug)]
pub struct Torrent {
    // url
    pub announce: String,
    pub info: Info,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct Info {
    pub name: String,
    /// The number of bytes in each piece the file is split into.
    ///
    /// For the purposes of transfer, files are split into fixed-size pieces which are all the same
    /// length except for possibly the last one which may be truncated. piece length is almost
    /// always a power of two, most commonly 2^18 = 256K (BitTorrent prior to version 3.2 uses 2
    /// 20 = 1 M as default).
    #[serde(rename = "piece length")]
    pub plength: usize,
    pub pieces: Hashes,
    #[serde(flatten)]
    pub keys: Keys,
}

#[derive(Deserialize, Serialize, Debug)]
#[serde(untagged)]
pub enum Keys {
    SingleFile { length: usize },
    MutilFile { files: Vec<File> },
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct File {
    pub length: usize,
    pub path: Vec<String>,
}

impl Torrent {
    pub fn info_hash(&self) -> [u8; 20] {
        let info_bytes = serde_bencode::to_bytes(&self.info).expect("re-encode to serde_bencode");
        let mut hasher = sha1::Sha1::new();
        hasher.update(&info_bytes);
        hasher.finalize().into()
    }

    pub fn print_tree(&self) {
        match &self.info.keys {
            Keys::SingleFile { length } => {
                println!("File length: {}", length);
            }
            Keys::MutilFile { files } => {
                for file in files {
                    println!("File length: {}", file.length);
                    println!("File path: {:?}", file.path);
                }
            }
        }
    }

    pub fn length(&self) -> usize {
        match &self.info.keys {
            Keys::SingleFile { length } => *length,
            Keys::MutilFile { files } => files.iter().map(|file| file.length).sum(),
        }
    }

    pub async fn read(file: impl AsRef<Path>) -> anyhow::Result<Self> {
        let dot_torrent = tokio::fs::read(file).await.context("read torrent file")?;
        let t: Torrent = serde_bencode::from_bytes(&dot_torrent).context("parse torrent file")?;
        Ok(t)
    }

    pub async fn download_all(&self) -> anyhow::Result<Downloaded> {
        download::all(self).await
    }
}

mod hashes {
    use serde::{de::Visitor, Deserialize, Serialize};

    #[derive(Debug, Clone)]
    pub struct Hashes(pub Vec<[u8; 20]>);

    struct HashVistor;
    impl<'de> Visitor<'de> for HashVistor {
        type Value = Hashes;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a byte string whose length is a multiple of 20")
        }

        fn visit_bytes<E>(self, v: &[u8]) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            if v.len() % 20 != 0 {
                return Err(serde::de::Error::invalid_length(v.len(), &self));
            }
            let mut data = Vec::new();
            for chunk in v.chunks(20) {
                let mut hash = [0; 20];
                hash.copy_from_slice(chunk);
                data.push(hash);
            }
            Ok(Hashes(data))
        }
    }

    impl<'de> Deserialize<'de> for Hashes {
        fn deserialize<D>(deserializer: D) -> Result<Hashes, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            deserializer.deserialize_bytes(HashVistor)
        }
    }

    impl Serialize for Hashes {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            let single_slice = self.0.concat();
            serializer.serialize_bytes(&single_slice)
        }
    }
}
