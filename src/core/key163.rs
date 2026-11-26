use aes::Aes128;
use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Deserializer, Serialize};

pub const KEY_163_PREFIX: &str = "163 key(Don't modify):";
const MUSIC_JSON_PREFIX: &str = "music:";
const NETEASE_163_KEY: &[u8; 16] = b"#14ljk_!\\]&0U<'(";

fn de_u64_flexible<'de, D>(deserializer: D) -> std::result::Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum U64OrStr {
        Int(u64),
        Str(String),
        Null,
    }

    match Option::<U64OrStr>::deserialize(deserializer)? {
        Some(U64OrStr::Int(v)) => Ok(v),
        Some(U64OrStr::Str(s)) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                Ok(0)
            } else {
                trimmed.parse::<u64>().map_err(serde::de::Error::custom)
            }
        }
        Some(U64OrStr::Null) | None => Ok(0),
    }
}

fn de_string_or_null<'de, D>(deserializer: D) -> std::result::Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(deserializer)?.unwrap_or_default())
}

fn de_vec_string_or_null<'de, D>(deserializer: D) -> std::result::Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Option::<Vec<String>>::deserialize(deserializer)?.unwrap_or_default())
}

fn de_artists_or_null<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<(String, u64)>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Option::<Vec<(String, u64)>>::deserialize(deserializer)?.unwrap_or_default())
}

/// JSON payload embedded inside NetEase Cloud Music `163 key(Don't modify):` comments.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetEase163Payload {
    #[serde(default, deserialize_with = "de_string_or_null")]
    pub format: String,
    #[serde(default, deserialize_with = "de_u64_flexible")]
    pub music_id: u64,
    #[serde(default, deserialize_with = "de_string_or_null")]
    pub music_name: String,
    #[serde(default, deserialize_with = "de_artists_or_null")]
    pub artist: Vec<(String, u64)>,
    #[serde(default, deserialize_with = "de_string_or_null")]
    pub album: String,
    #[serde(default, deserialize_with = "de_u64_flexible")]
    pub album_id: u64,
    #[serde(default, deserialize_with = "de_string_or_null")]
    pub album_pic: String,
    #[serde(default, deserialize_with = "de_u64_flexible")]
    pub album_pic_doc_id: u64,
    #[serde(default, deserialize_with = "de_u64_flexible")]
    pub mv_id: u64,
    #[serde(default, deserialize_with = "de_u64_flexible")]
    pub flag: u64,
    #[serde(default, deserialize_with = "de_vec_string_or_null")]
    pub alias: Vec<String>,
    #[serde(default, deserialize_with = "de_vec_string_or_null")]
    pub trans_names: Vec<String>,
    #[serde(default, deserialize_with = "de_u64_flexible")]
    pub bitrate: u64,
    #[serde(default, deserialize_with = "de_u64_flexible")]
    pub duration: u64,
}

/// Encodes a `NetEase163Payload` into a `163 key(Don't modify):...` comment string.
pub fn encode_163_key(payload: &NetEase163Payload) -> Result<String> {
    let json = serde_json::to_string(payload).context("failed to serialize 163 key payload")?;
    let mut plaintext = Vec::with_capacity(MUSIC_JSON_PREFIX.len() + json.len() + 16);
    plaintext.extend_from_slice(MUSIC_JSON_PREFIX.as_bytes());
    plaintext.extend_from_slice(json.as_bytes());

    let pad_len = 16 - (plaintext.len() % 16);
    plaintext.resize(plaintext.len() + pad_len, pad_len as u8);

    let cipher = Aes128::new(GenericArray::from_slice(NETEASE_163_KEY));
    for chunk in plaintext.chunks_exact_mut(16) {
        let block = GenericArray::from_mut_slice(chunk);
        cipher.encrypt_block(block);
    }

    let b64 = STANDARD.encode(&plaintext);
    Ok(format!("{KEY_163_PREFIX}{b64}"))
}

/// Decodes a `163 key(Don't modify):...` string (or a comment containing it) into `NetEase163Payload`.
pub fn decode_163_key(input: &str) -> Result<NetEase163Payload> {
    let start = input
        .find(KEY_163_PREFIX)
        .context("missing '163 key(Don't modify):' prefix")?;
    let rest = &input[start + KEY_163_PREFIX.len()..];
    let b64_token = rest
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim();
    if b64_token.is_empty() {
        bail!("empty 163 key base64 payload");
    }

    let mut data = STANDARD
        .decode(b64_token)
        .context("invalid base64 in 163 key")?;
    if data.is_empty() || data.len() % 16 != 0 {
        bail!("invalid AES-128 block length in 163 key: {}", data.len());
    }

    let cipher = Aes128::new(GenericArray::from_slice(NETEASE_163_KEY));
    for chunk in data.chunks_exact_mut(16) {
        let block = GenericArray::from_mut_slice(chunk);
        cipher.decrypt_block(block);
    }

    let pad_len = *data.last().context("empty decrypted 163 key")? as usize;
    if pad_len == 0 || pad_len > 16 || pad_len > data.len() {
        bail!("invalid PKCS7 padding length: {pad_len}");
    }
    if !data[data.len() - pad_len..].iter().all(|&b| b as usize == pad_len) {
        bail!("corrupted PKCS7 padding in 163 key");
    }
    data.truncate(data.len() - pad_len);

    let decoded_str = std::str::from_utf8(&data).context("163 key payload is not valid UTF-8")?;
    let json_str = decoded_str
        .strip_prefix(MUSIC_JSON_PREFIX)
        .context("decrypted 163 key missing 'music:' prefix")?;

    serde_json::from_str::<NetEase163Payload>(json_str)
        .context("failed to parse decrypted 163 key JSON")
}

/// Extracts the NetEase `musicId` as a string if `comment` contains a valid `163 key`.
pub fn extract_163_song_id(comment: &str) -> Option<String> {
    let payload = decode_163_key(comment).ok()?;
    if payload.music_id == 0 {
        None
    } else {
        Some(payload.music_id.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aes_key_length_is_16_bytes() {
        assert_eq!(NETEASE_163_KEY.len(), 16);
    }

    #[test]
    fn roundtrip_163_key_1357953768() {
        let payload = NetEase163Payload {
            format: "flac".to_string(),
            music_id: 1357953768,
            music_name: "だから僕は音楽を辞めた".to_string(),
            artist: vec![("ヨルシカ".to_string(), 28082342)],
            album: "だから僕は音楽を辞めた".to_string(),
            album_id: 78436910,
            album_pic: "https://p1.music.126.net/example/109951163963487312.jpg".to_string(),
            album_pic_doc_id: 109951163963487312,
            mv_id: 10862091,
            flag: 4,
            alias: vec![],
            trans_names: vec!["所以我放弃了音乐".to_string()],
            bitrate: 864000,
            duration: 242000,
        };

        let encoded = encode_163_key(&payload).expect("encode should succeed");
        assert!(encoded.starts_with(KEY_163_PREFIX));

        let decoded = decode_163_key(&encoded).expect("decode should succeed");
        assert_eq!(decoded, payload);
        assert_eq!(extract_163_song_id(&encoded).as_deref(), Some("1357953768"));
    }

    #[test]
    fn decode_real_netease_json_structure_3455171774_with_string_doc_id() {
        let raw_json = r#"{"format":"mp3","musicId":3455171774,"musicName":"Sample Track","artist":[["Artist A",1001],["Artist B",1002]],"album":"Sample Album","albumId":998877,"albumPic":"http://p3.music.126.net/abc.jpg","albumPicDocId":"109951165432109876","mvId":0,"flag":128,"alias":["Alt Name"],"transNames":["译名"],"bitrate":320000,"duration":215000}"#;

        let mut plaintext = format!("music:{raw_json}").into_bytes();
        let pad_len = 16 - (plaintext.len() % 16);
        plaintext.resize(plaintext.len() + pad_len, pad_len as u8);
        let cipher = Aes128::new(GenericArray::from_slice(NETEASE_163_KEY));
        for chunk in plaintext.chunks_exact_mut(16) {
            cipher.encrypt_block(GenericArray::from_mut_slice(chunk));
        }
        let comment = format!(
            "User note\n{}{}",
            KEY_163_PREFIX,
            STANDARD.encode(&plaintext)
        );

        let decoded = decode_163_key(&comment).expect("should decode embedded 163 key");
        assert_eq!(decoded.music_id, 3455171774);
        assert_eq!(decoded.music_name, "Sample Track");
        assert_eq!(
            decoded.artist,
            vec![("Artist A".to_string(), 1001), ("Artist B".to_string(), 1002)]
        );
        assert_eq!(decoded.album_pic_doc_id, 109951165432109876);
        assert_eq!(decoded.alias, vec!["Alt Name".to_string()]);
        assert_eq!(decoded.trans_names, vec!["译名".to_string()]);
        assert_eq!(extract_163_song_id(&comment).as_deref(), Some("3455171774"));

        // Re-encode and verify JSON camelCase keys match NetEase format.
        let re_encoded = encode_163_key(&decoded).expect("re-encode");
        let re_decoded = decode_163_key(&re_encoded).expect("re-decode");
        assert_eq!(re_decoded, decoded);

        let serialized_val: serde_json::Value =
            serde_json::to_value(&decoded).expect("serialize to value");
        assert!(serialized_val.get("musicId").is_some());
        assert!(serialized_val.get("musicName").is_some());
        assert!(serialized_val.get("albumId").is_some());
        assert!(serialized_val.get("albumPic").is_some());
        assert!(serialized_val.get("albumPicDocId").is_some());
        assert!(serialized_val.get("mvId").is_some());
        assert!(serialized_val.get("transNames").is_some());
    }

    #[test]
    fn invalid_163_key_errors_cleanly() {
        assert!(decode_163_key("ordinary comment").is_err());
        assert!(decode_163_key("163 key(Don't modify):not-valid-base64!!!").is_err());
        assert!(extract_163_song_id("ordinary comment").is_none());
    }
}
