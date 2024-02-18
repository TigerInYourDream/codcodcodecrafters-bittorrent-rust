use anyhow::{anyhow, Result};
use serde_bencode;
use serde_json;
use std::env;

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
fn main() {
    let args: Vec<String> = env::args().collect();
    let command = &args[1];

    if command == "decode" {
        // Uncomment this block to pass the first stage
        let encoded_value = &args[2];
        let decoded_value = decode(encoded_value);
        eprintln!("{:?}", decoded_value);
        match decoded_value {
            Ok(value) => {
                eprintln!("{}", value.to_string());
            }
            Err(e) => {
                eprintln!("Error: {}", e);
            }
        }
    } else {
        eprintln!("unknown command: {}", args[1])
    }
}
