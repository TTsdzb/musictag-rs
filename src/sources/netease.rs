use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::FixedOffset;
use reqwest::blocking::Client;
use reqwest::header::{HeaderMap, HeaderValue, REFERER, USER_AGENT};
use serde::{Deserialize, Deserializer};

use crate::core::key163::NetEase163Payload;
use crate::core::lyrics::process_netease_lyrics;
use crate::core::models::{
    CandidateTrack, CoverSearchItem, LyricData, LyricProcessOptions, ProcessedLyrics, SearchQuery,
    SourceCapabilities, SourceInfo,
};
use crate::core::traits::TagSource;

const NETEASE_SOURCE_ID: &str = "netease";
const NETEASE_SOURCE_NAME: &str = "网易云音乐";
const NETEASE_REFERER: &str = "https://music.163.com/";
const DESKTOP_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36";

const CLOUDSEARCH_URL: &str = "https://music.163.com/api/cloudsearch/pc";
const SEARCH_WEB_URL: &str = "https://music.163.com/api/search/get/web";
const DETAIL_V3_URL: &str = "https://music.163.com/api/v3/song/detail";
const DETAIL_V1_URL: &str = "https://music.163.com/api/song/detail/";
const LYRIC_URL: &str = "https://music.163.com/api/song/lyric";

/// Built-in NetEase Cloud Music (`music.163.com`) metadata, lyrics, and cover source.
#[derive(Debug, Clone)]
pub struct NetEaseSource {
    client: Client,
    artist_sep: String,
}

impl Default for NetEaseSource {
    fn default() -> Self {
        Self::new(";")
    }
}

impl NetEaseSource {
    /// Creates a new `NetEaseSource` using the given artist separator (defaults to `";"` if empty).
    pub fn new(artist_sep: impl Into<String>) -> Self {
        let sep = artist_sep.into();
        let artist_sep = if sep.is_empty() { ";".to_string() } else { sep };
        let client = build_netease_client().unwrap_or_else(|_| Client::new());
        Self { client, artist_sep }
    }

    pub fn artist_sep(&self) -> &str {
        &self.artist_sep
    }

    fn search_cloudsearch(&self, keyword: &str, limit: usize) -> Result<Vec<RawNetEaseSong>> {
        let limit_str = limit.to_string();
        let resp = self
            .client
            .post(CLOUDSEARCH_URL)
            .header(USER_AGENT, DESKTOP_USER_AGENT)
            .header(REFERER, NETEASE_REFERER)
            .form(&[
                ("s", keyword),
                ("type", "1"),
                ("offset", "0"),
                ("limit", limit_str.as_str()),
                ("total", "true"),
            ])
            .send()
            .context("NetEase cloudsearch request failed")?
            .error_for_status()
            .context("NetEase cloudsearch returned HTTP error")?;

        let body = resp
            .text()
            .context("failed to read NetEase cloudsearch response body")?;
        parse_search_response(&body)
    }

    fn search_web_fallback(&self, keyword: &str, limit: usize) -> Result<Vec<RawNetEaseSong>> {
        let limit_str = limit.to_string();
        let resp = self
            .client
            .post(SEARCH_WEB_URL)
            .header(USER_AGENT, DESKTOP_USER_AGENT)
            .header(REFERER, NETEASE_REFERER)
            .form(&[
                ("s", keyword),
                ("type", "1"),
                ("offset", "0"),
                ("limit", limit_str.as_str()),
                ("total", "true"),
            ])
            .send()
            .context("NetEase web search fallback request failed")?
            .error_for_status()
            .context("NetEase web search fallback returned HTTP error")?;

        let body = resp
            .text()
            .context("failed to read NetEase web search response body")?;
        parse_search_response(&body)
    }

    fn fetch_detail_v3(&self, id: &str) -> Result<RawNetEaseSong> {
        let c_param = format!("[{{\"id\":{id}}}]");
        let resp = self
            .client
            .get(DETAIL_V3_URL)
            .header(USER_AGENT, DESKTOP_USER_AGENT)
            .header(REFERER, NETEASE_REFERER)
            .query(&[("c", c_param.as_str())])
            .send()
            .context("NetEase v3 song detail request failed")?
            .error_for_status()
            .context("NetEase v3 song detail returned HTTP error")?;

        let body = resp
            .text()
            .context("failed to read NetEase v3 song detail response body")?;
        let mut songs = parse_detail_response(&body)?;
        songs
            .drain(..)
            .next()
            .with_context(|| format!("NetEase v3 detail returned no song for id {id}"))
    }

    fn fetch_detail_v1(&self, id: &str) -> Result<RawNetEaseSong> {
        let ids_param = format!("[{id}]");
        let resp = self
            .client
            .get(DETAIL_V1_URL)
            .header(USER_AGENT, DESKTOP_USER_AGENT)
            .header(REFERER, NETEASE_REFERER)
            .query(&[("id", id), ("ids", ids_param.as_str())])
            .send()
            .context("NetEase legacy song detail request failed")?
            .error_for_status()
            .context("NetEase legacy song detail returned HTTP error")?;

        let body = resp
            .text()
            .context("failed to read NetEase legacy song detail response body")?;
        let mut songs = parse_detail_response(&body)?;
        songs
            .drain(..)
            .next()
            .with_context(|| format!("NetEase legacy detail returned no song for id {id}"))
    }
}

impl TagSource for NetEaseSource {
    fn info(&self) -> SourceInfo {
        SourceInfo {
            id: NETEASE_SOURCE_ID.to_string(),
            name: NETEASE_SOURCE_NAME.to_string(),
            version: "1.0.0".to_string(),
            description: "NetEase Cloud Music metadata, lyrics, high-res covers, and 163 key"
                .to_string(),
            builtin: true,
            enabled: true,
            capabilities: SourceCapabilities {
                combo: true,
                lyrics: true,
                cover: true,
                key163: true,
            },
        }
    }

    fn search_combo(&self, query: &SearchQuery) -> Result<Vec<CandidateTrack>> {
        if let Some(raw_id) = query.song_id.as_deref() {
            let trimmed = raw_id.trim();
            if !trimmed.is_empty() {
                let detail = self.fetch_detail(trimmed)?;
                return Ok(vec![detail]);
            }
        }

        let keyword = build_search_keyword(query);
        if keyword.is_empty() {
            return Ok(Vec::new());
        }

        let limit = if query.limit == 0 {
            10
        } else {
            query.limit.min(100)
        };

        let songs = match self.search_cloudsearch(&keyword, limit) {
            Ok(list) if !list.is_empty() => list,
            Ok(_) => self.search_web_fallback(&keyword, limit).unwrap_or_default(),
            Err(primary_err) => self
                .search_web_fallback(&keyword, limit)
                .with_context(|| format!("cloudsearch failed ({primary_err:#})"))?,
        };

        Ok(songs
            .into_iter()
            .map(|song| song_to_candidate(song, &self.artist_sep))
            .collect())
    }

    fn fetch_detail(&self, song_id: &str) -> Result<CandidateTrack> {
        let id = parse_netease_song_id(song_id)
            .with_context(|| format!("invalid NetEase song id or URL: {song_id:?}"))?;

        let raw_song = match self.fetch_detail_v3(&id) {
            Ok(song) => song,
            Err(v3_err) => self
                .fetch_detail_v1(&id)
                .with_context(|| format!("v3 detail failed ({v3_err:#})"))?,
        };

        let mut candidate = song_to_candidate(raw_song, &self.artist_sep);
        if let Ok(lyrics) = self.fetch_lyrics(&id) {
            apply_lyrics_to_candidate(&mut candidate, lyrics);
        }
        Ok(candidate)
    }

    fn fetch_lyrics(&self, song_id: &str) -> Result<LyricData> {
        let id = parse_netease_song_id(song_id)
            .with_context(|| format!("invalid NetEase song id or URL: {song_id:?}"))?;

        let resp = self
            .client
            .get(LYRIC_URL)
            .header(USER_AGENT, DESKTOP_USER_AGENT)
            .header(REFERER, NETEASE_REFERER)
            .query(&[
                ("id", id.as_str()),
                ("lv", "-1"),
                ("kv", "-1"),
                ("tv", "-1"),
            ])
            .send()
            .context("NetEase lyric request failed")?
            .error_for_status()
            .context("NetEase lyric returned HTTP error")?;

        let body = resp
            .text()
            .context("failed to read NetEase lyric response body")?;
        parse_lyric_response(&body)
    }

    fn search_cover(&self, query: &SearchQuery) -> Result<Vec<CoverSearchItem>> {
        let candidates = self.search_combo(query)?;
        let mut seen_urls = HashSet::new();
        let mut items = Vec::new();

        for candidate in candidates {
            let Some(url) = candidate.cover_url else {
                continue;
            };
            if url.is_empty() || !seen_urls.insert(url.clone()) {
                continue;
            }
            let title = if !candidate.album.is_empty() {
                candidate.album
            } else {
                candidate.title
            };
            items.push(CoverSearchItem {
                source_id: NETEASE_SOURCE_ID.to_string(),
                source_name: NETEASE_SOURCE_NAME.to_string(),
                title,
                format: infer_image_format(&url),
                url,
                width: None,
                height: None,
                size_bytes: None,
                thumb_bytes: None,
            });
        }

        Ok(items)
    }

    fn download_bytes(&self, url: &str) -> Result<Vec<u8>> {
        let trimmed = url.trim();
        if trimmed.is_empty() {
            bail!("cannot download bytes from empty URL");
        }
        let resp = self
            .client
            .get(trimmed)
            .header(USER_AGENT, DESKTOP_USER_AGENT)
            .header(REFERER, NETEASE_REFERER)
            .send()
            .with_context(|| format!("failed to download image from {trimmed}"))?
            .error_for_status()
            .with_context(|| format!("HTTP error downloading image from {trimmed}"))?;
        let bytes = resp
            .bytes()
            .with_context(|| format!("failed to read response bytes from {trimmed}"))?;
        Ok(bytes.to_vec())
    }
}

fn build_netease_client() -> Result<Client> {
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static(DESKTOP_USER_AGENT));
    headers.insert(REFERER, HeaderValue::from_static(NETEASE_REFERER));
    Client::builder()
        .timeout(Duration::from_secs(15))
        .default_headers(headers)
        .build()
        .context("failed to build NetEase reqwest blocking client")
}

/// Extracts a numeric NetEase song ID from either a raw ID (`"1357953768"`) or a `music.163.com` URL.
pub fn parse_netease_song_id(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.bytes().all(|b| b.is_ascii_digit()) {
        return Some(trimmed.to_string());
    }

    for marker in ["?id=", "&id="] {
        if let Some(pos) = trimmed.find(marker) {
            let after = &trimmed[pos + marker.len()..];
            let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
            if !digits.is_empty() {
                return Some(digits);
            }
        }
    }

    if let Some(pos) = trimmed.find("/song/") {
        let after = &trimmed[pos + "/song/".len()..];
        let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() {
            return Some(digits);
        }
    }

    None
}

/// Strips NetEase CDN resizing parameters (e.g. `?param=130y130`) so full-resolution artwork downloads.
pub fn strip_netease_pic_params(url: &str) -> String {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if let Some((base, query)) = trimmed.split_once('?') {
        if query.contains("param=")
            || query.contains("thumbnail=")
            || query.contains("imageView")
            || base.contains("music.126.net")
        {
            return base.to_string();
        }
    }
    trimmed.to_string()
}

/// Extracts `albumPicDocId` from `pic`, `pic_str`, or the numeric filename stem before `.jpg` in `picUrl`.
pub fn extract_pic_doc_id(pic: Option<u64>, pic_str: Option<&str>, pic_url: &str) -> u64 {
    if let Some(v) = pic {
        if v > 0 {
            return v;
        }
    }
    if let Some(s) = pic_str {
        if let Ok(v) = s.trim().parse::<u64>() {
            if v > 0 {
                return v;
            }
        }
    }

    let clean = strip_netease_pic_params(pic_url);
    let last_segment = clean.rsplit('/').next().unwrap_or("");
    let stem = last_segment.split('.').next().unwrap_or("");
    if !stem.is_empty() && stem.bytes().all(|b| b.is_ascii_digit()) {
        stem.parse::<u64>().unwrap_or(0)
    } else {
        0
    }
}

fn build_search_keyword(query: &SearchQuery) -> String {
    [
        query.title.trim(),
        query.artist.trim(),
        query.album.trim(),
    ]
    .into_iter()
    .filter(|part| !part.is_empty())
    .collect::<Vec<_>>()
    .join(" ")
}

fn infer_image_format(url: &str) -> String {
    let clean = strip_netease_pic_params(url).to_ascii_lowercase();
    if clean.ends_with(".png") {
        "PNG".to_string()
    } else if clean.ends_with(".webp") {
        "WEBP".to_string()
    } else if clean.ends_with(".gif") {
        "GIF".to_string()
    } else {
        "JPEG".to_string()
    }
}

fn format_publish_year(publish_time_ms: Option<i64>) -> String {
    let Some(ms) = publish_time_ms else {
        return String::new();
    };
    if ms <= 0 {
        return String::new();
    }
    // NetEase publishTime stamps align with Beijing time (UTC+8).
    let Some(offset) = FixedOffset::east_opt(8 * 3600) else {
        return String::new();
    };
    let Some(utc_dt) = chrono::DateTime::from_timestamp_millis(ms) else {
        return String::new();
    };
    utc_dt.with_timezone(&offset).format("%Y").to_string()
}

fn title_looks_instrumental(title: &str) -> bool {
    let lower = title.to_ascii_lowercase();
    lower.contains("instrumental")
        || lower.contains("off vocal")
        || title.contains("伴奏")
        || title.contains("纯音乐")
}

fn song_to_candidate(song: RawNetEaseSong, artist_sep: &str) -> CandidateTrack {
    let artists_raw = song.ar.or(song.artists).unwrap_or_default();
    let artist_pairs: Vec<(String, u64)> = artists_raw
        .into_iter()
        .filter(|a| !a.name.trim().is_empty())
        .map(|a| (a.name.trim().to_string(), a.id))
        .collect();
    let artist_joined = artist_pairs
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(artist_sep);

    let album_obj = song.al.or(song.album);
    let (album_name, album_id, album_pic, album_pic_doc_id, album_publish_time, album_artist) =
        if let Some(al) = album_obj {
            let clean_pic = al
                .pic_url
                .as_deref()
                .map(strip_netease_pic_params)
                .filter(|s| !s.is_empty())
                .unwrap_or_default();
            let doc_id = extract_pic_doc_id(al.pic, al.pic_str.as_deref(), &clean_pic);

            let mut al_artists: Vec<String> = al
                .artists
                .unwrap_or_default()
                .into_iter()
                .map(|a| a.name.trim().to_string())
                .filter(|n| !n.is_empty())
                .collect();
            if al_artists.is_empty() {
                if let Some(single) = al.artist {
                    if !single.name.trim().is_empty() {
                        al_artists.push(single.name.trim().to_string());
                    }
                }
            }
            let al_artist_str = if al_artists.is_empty() {
                String::new()
            } else {
                let joined = al_artists.join(artist_sep);
                if joined.eq_ignore_ascii_case(&artist_joined) {
                    String::new()
                } else {
                    joined
                }
            };

            (
                al.name.trim().to_string(),
                al.id,
                clean_pic,
                doc_id,
                al.publish_time,
                al_artist_str,
            )
        } else {
            (String::new(), 0, String::new(), 0, None, String::new())
        };

    let duration_ms = song.dt.or(song.duration).unwrap_or(0);
    let track_num = song.no.or(song.position).unwrap_or(0);
    let track = if track_num > 0 {
        track_num.to_string()
    } else {
        String::new()
    };

    let disc = normalize_netease_disc(song.cd.or(song.disc));

    let year = format_publish_year(song.publish_time.or(album_publish_time));
    let mv_id = song.mv.or(song.mvid).unwrap_or(0);
    let alias = song.alia.or(song.alias).unwrap_or_default();
    let trans_names = song.tns.or(song.trans_names).unwrap_or_default();
    let music_name = song.name.trim().to_string();

    let netease_meta = NetEase163Payload {
        format: "mp3".to_string(),
        music_id: song.id,
        music_name: music_name.clone(),
        artist: artist_pairs,
        album: album_name.clone(),
        album_id,
        album_pic: album_pic.clone(),
        album_pic_doc_id,
        mv_id,
        flag: 4,
        alias,
        trans_names,
        bitrate: 320_000,
        duration: duration_ms,
    };

    let is_instrumental = title_looks_instrumental(&music_name);

    CandidateTrack {
        source_id: NETEASE_SOURCE_ID.to_string(),
        source_name: NETEASE_SOURCE_NAME.to_string(),
        song_id: song.id.to_string(),
        title: music_name,
        artist: artist_joined,
        album: album_name,
        album_artist,
        year,
        track,
        disc,
        genre: String::new(),
        composer: String::new(),
        lyricist: String::new(),
        comment: String::new(),
        duration_ms,
        cover_url: if album_pic.is_empty() {
            None
        } else {
            Some(album_pic)
        },
        cover_size: String::new(),
        cover_bytes: None,
        netease_meta: Some(netease_meta),
        raw_lrc: None,
        raw_tlyric: None,
        nolyric: false,
        is_instrumental,
        has_translation: false,
        lyric_preview: String::new(),
    }
}

fn apply_lyrics_to_candidate(candidate: &mut CandidateTrack, lyrics: LyricData) {
    let has_translation = lyrics
        .raw_tlyric
        .as_deref()
        .is_some_and(|s| !s.trim().is_empty());

    let processed = process_netease_lyrics(
        lyrics.raw_lrc.as_deref(),
        lyrics.raw_tlyric.as_deref(),
        lyrics.nolyric,
        &candidate.title,
        &LyricProcessOptions::default(),
    );

    match processed {
        ProcessedLyrics::Lyrics(text) => {
            candidate.lyric_preview = text
                .lines()
                .map(|l| l.trim_end_matches('\r'))
                .filter(|l| !l.trim().is_empty())
                .take(4)
                .collect::<Vec<_>>()
                .join("\n");
        }
        ProcessedLyrics::InstrumentalSkipped => {
            candidate.is_instrumental = true;
            candidate.lyric_preview = "(纯音乐 · 无歌词)".to_string();
        }
        ProcessedLyrics::Empty => {}
    }

    candidate.nolyric = lyrics.nolyric;
    candidate.has_translation = has_translation;
    candidate.raw_lrc = lyrics.raw_lrc;
    candidate.raw_tlyric = lyrics.raw_tlyric;
}

fn value_to_trimmed_string(val: serde_json::Value) -> String {
    match val {
        serde_json::Value::String(s) => s.trim().to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

fn normalize_netease_disc(raw: Option<serde_json::Value>) -> String {
    let trimmed = raw.map(value_to_trimmed_string).unwrap_or_default();
    if trimmed.is_empty() {
        return String::new();
    }
    if let Some((num_part, total_part)) = trimmed.split_once('/') {
        let num_str = num_part.trim();
        let total_str = total_part.trim();
        if let (Ok(num), Ok(total)) = (num_str.parse::<u32>(), total_str.parse::<u32>()) {
            if num == 0 || (num <= 1 && total <= 1) {
                return String::new();
            }
            return trimmed;
        }
        return trimmed;
    }
    if trimmed.bytes().all(|b| b.is_ascii_digit()) {
        if let Ok(n) = trimmed.parse::<u32>() {
            return if n <= 1 {
                String::new()
            } else {
                n.to_string()
            };
        }
    }
    trimmed
}

/// Parses a NetEase search JSON payload (`/api/cloudsearch/pc` or `/api/search/get/web`).
pub fn parse_search_response(json_str: &str) -> Result<Vec<RawNetEaseSong>> {
    let parsed: RawSearchEnvelope =
        serde_json::from_str(json_str).context("failed to parse NetEase search JSON")?;
    if let Some(code) = parsed.code {
        if code != 200 {
            bail!("NetEase search API returned code {code}");
        }
    }
    Ok(parsed.result.and_then(|r| r.songs).unwrap_or_default())
}

/// Parses a NetEase song detail JSON payload (`/api/v3/song/detail` or `/api/song/detail/`).
pub fn parse_detail_response(json_str: &str) -> Result<Vec<RawNetEaseSong>> {
    let parsed: RawDetailEnvelope =
        serde_json::from_str(json_str).context("failed to parse NetEase song detail JSON")?;
    if let Some(code) = parsed.code {
        if code != 200 {
            bail!("NetEase detail API returned code {code}");
        }
    }
    Ok(parsed.songs.unwrap_or_default())
}

/// Parses a NetEase lyric JSON payload (`/api/song/lyric`).
pub fn parse_lyric_response(json_str: &str) -> Result<LyricData> {
    let parsed: RawLyricEnvelope =
        serde_json::from_str(json_str).context("failed to parse NetEase lyric JSON")?;
    if let Some(code) = parsed.code {
        if code != 200 {
            bail!("NetEase lyric API returned code {code}");
        }
    }
    let raw_lrc = parsed
        .lrc
        .and_then(|b| b.lyric)
        .filter(|s| !s.trim().is_empty());
    let raw_tlyric = parsed
        .tlyric
        .and_then(|b| b.lyric)
        .filter(|s| !s.trim().is_empty());

    Ok(LyricData {
        raw_lrc,
        raw_tlyric,
        nolyric: parsed.nolyric.unwrap_or(false),
        uncollected: parsed.uncollected.unwrap_or(false),
    })
}

fn de_opt_u64_flexible<'de, D>(deserializer: D) -> std::result::Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OptNum {
        Int(u64),
        Str(String),
        Null,
    }

    match Option::<OptNum>::deserialize(deserializer)? {
        Some(OptNum::Int(v)) => Ok(Some(v)),
        Some(OptNum::Str(s)) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                Ok(trimmed.parse::<u64>().ok())
            }
        }
        Some(OptNum::Null) | None => Ok(None),
    }
}

fn de_string_or_null<'de, D>(deserializer: D) -> std::result::Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Debug, Deserialize)]
struct RawSearchEnvelope {
    code: Option<i64>,
    result: Option<RawSearchResult>,
}

#[derive(Debug, Deserialize)]
struct RawSearchResult {
    songs: Option<Vec<RawNetEaseSong>>,
}

#[derive(Debug, Deserialize)]
struct RawDetailEnvelope {
    code: Option<i64>,
    songs: Option<Vec<RawNetEaseSong>>,
}

#[derive(Debug, Deserialize)]
struct RawLyricEnvelope {
    code: Option<i64>,
    lrc: Option<RawLyricBlock>,
    tlyric: Option<RawLyricBlock>,
    nolyric: Option<bool>,
    uncollected: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct RawLyricBlock {
    lyric: Option<String>,
}

/// Raw song object matching both NetEase v3/cloudsearch (`ar`/`al`/`dt`) and v1 (`artists`/`album`/`duration`).
#[derive(Debug, Deserialize)]
pub struct RawNetEaseSong {
    pub id: u64,
    #[serde(default, deserialize_with = "de_string_or_null")]
    pub name: String,
    pub ar: Option<Vec<RawNetEaseArtist>>,
    pub artists: Option<Vec<RawNetEaseArtist>>,
    pub al: Option<RawNetEaseAlbum>,
    pub album: Option<RawNetEaseAlbum>,
    pub dt: Option<u64>,
    pub duration: Option<u64>,
    pub no: Option<u64>,
    pub position: Option<u64>,
    pub cd: Option<serde_json::Value>,
    pub disc: Option<serde_json::Value>,
    pub mv: Option<u64>,
    pub mvid: Option<u64>,
    pub alia: Option<Vec<String>>,
    pub alias: Option<Vec<String>>,
    pub tns: Option<Vec<String>>,
    #[serde(rename = "transNames")]
    pub trans_names: Option<Vec<String>>,
    #[serde(rename = "publishTime")]
    pub publish_time: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct RawNetEaseArtist {
    #[serde(default)]
    pub id: u64,
    #[serde(default, deserialize_with = "de_string_or_null")]
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct RawNetEaseAlbum {
    #[serde(default)]
    pub id: u64,
    #[serde(default, deserialize_with = "de_string_or_null")]
    pub name: String,
    #[serde(rename = "picUrl")]
    pub pic_url: Option<String>,
    #[serde(default, deserialize_with = "de_opt_u64_flexible")]
    pub pic: Option<u64>,
    pub pic_str: Option<String>,
    #[serde(rename = "publishTime")]
    pub publish_time: Option<i64>,
    pub artists: Option<Vec<RawNetEaseArtist>>,
    pub artist: Option<RawNetEaseArtist>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::key163::{decode_163_key, encode_163_key};

    #[test]
    fn parse_song_id_handles_digits_and_urls() {
        assert_eq!(
            parse_netease_song_id("1357953768").as_deref(),
            Some("1357953768")
        );
        assert_eq!(
            parse_netease_song_id("  1357953768  ").as_deref(),
            Some("1357953768")
        );
        assert_eq!(
            parse_netease_song_id("https://music.163.com/song?id=1357953768").as_deref(),
            Some("1357953768")
        );
        assert_eq!(
            parse_netease_song_id("https://music.163.com/#/song?id=1357953768&userid=42")
                .as_deref(),
            Some("1357953768")
        );
        assert_eq!(
            parse_netease_song_id("https://music.163.com/song/1357953768/").as_deref(),
            Some("1357953768")
        );
        assert!(parse_netease_song_id("").is_none());
        assert!(parse_netease_song_id("not-an-id").is_none());
    }

    #[test]
    fn strip_pic_params_and_extract_doc_id() {
        let raw_url =
            "https://p1.music.126.net/6y-UleORITEDbvrOLV0Q8A==/109951163963487312.jpg?param=130y130";
        let clean = strip_netease_pic_params(raw_url);
        assert_eq!(
            clean,
            "https://p1.music.126.net/6y-UleORITEDbvrOLV0Q8A==/109951163963487312.jpg"
        );
        assert_eq!(extract_pic_doc_id(Some(0), None, raw_url), 109951163963487312);
        assert_eq!(
            extract_pic_doc_id(Some(998877), None, raw_url),
            998877
        );
        assert_eq!(
            extract_pic_doc_id(None, Some("109951160000000001"), raw_url),
            109951160000000001
        );
    }

    #[test]
    fn parse_cloudsearch_and_detail_json_populates_candidate_and_163_meta() {
        let search_json = r#"{
            "code": 200,
            "result": {
                "songCount": 1,
                "songs": [
                    {
                        "id": 1357953768,
                        "name": "だから僕は音楽を辞めた",
                        "ar": [
                            {"id": 28082342, "name": "ヨルシカ"},
                            {"id": 12345, "name": "n-buna"}
                        ],
                        "al": {
                            "id": 78436910,
                            "name": "だから僕は音楽を辞めた",
                            "picUrl": "https://p2.music.126.net/abc/109951163963487312.jpg?param=200y200",
                            "pic": 0,
                            "publishTime": 1554825600000
                        },
                        "dt": 242000,
                        "no": 14,
                        "cd": "01",
                        "mv": 10862091,
                        "alia": ["Album Version"],
                        "tns": ["所以我放弃了音乐"]
                    }
                ]
            }
        }"#;

        let songs = parse_search_response(search_json).expect("search JSON should parse");
        assert_eq!(songs.len(), 1);
        let candidate = song_to_candidate(songs.into_iter().next().unwrap(), ";");
        assert_eq!(candidate.source_id, "netease");
        assert_eq!(candidate.song_id, "1357953768");
        assert_eq!(candidate.title, "だから僕は音楽を辞めた");
        assert_eq!(candidate.artist, "ヨルシカ;n-buna");
        assert_eq!(candidate.album, "だから僕は音楽を辞めた");
        assert_eq!(candidate.album_artist, "");
        assert_eq!(candidate.year, "2019");
        assert_eq!(candidate.track, "14");
        assert_eq!(candidate.disc, "");
        assert_eq!(candidate.duration_ms, 242000);
        assert_eq!(
            candidate.cover_url.as_deref(),
            Some("https://p2.music.126.net/abc/109951163963487312.jpg")
        );

        let meta = candidate.netease_meta.expect("netease_meta populated");
        assert_eq!(meta.format, "mp3");
        assert_eq!(meta.music_id, 1357953768);
        assert_eq!(meta.music_name, "だから僕は音楽を辞めた");
        assert_eq!(
            meta.artist,
            vec![
                ("ヨルシカ".to_string(), 28082342),
                ("n-buna".to_string(), 12345)
            ]
        );
        assert_eq!(meta.album_id, 78436910);
        assert_eq!(meta.album_pic_doc_id, 109951163963487312);
        assert_eq!(meta.mv_id, 10862091);
        assert_eq!(meta.flag, 4);
        assert_eq!(meta.alias, vec!["Album Version".to_string()]);
        assert_eq!(meta.trans_names, vec!["所以我放弃了音乐".to_string()]);
        assert_eq!(meta.bitrate, 320_000);
        assert_eq!(meta.duration, 242000);

        let encoded = encode_163_key(&meta).expect("163 key encodes");
        let decoded = decode_163_key(&encoded).expect("163 key roundtrips");
        assert_eq!(decoded, meta);
    }

    #[test]
    fn parse_legacy_v1_detail_and_lyric_json() {
        let v1_detail = r#"{
            "code": 200,
            "songs": [
                {
                    "id": 425137083,
                    "name": "ウミユリ海底譚",
                    "artists": [{"id": 159300, "name": "n-buna"}],
                    "album": {
                        "id": 34817090,
                        "name": "「マジカルミライ 2016」OFFICIAL ALBUM",
                        "picUrl": "http://p3.music.126.net/xyz/18571850906423456.jpg",
                        "pic": 18571850906423456,
                        "publishTime": 1470153600000,
                        "artists": [{"id": 0, "name": "Various Artists"}]
                    },
                    "duration": 240000,
                    "position": 3,
                    "disc": "1",
                    "mvid": 0,
                    "alias": [],
                    "transNames": null
                }
            ]
        }"#;

        let songs = parse_detail_response(v1_detail).expect("v1 detail parses");
        let mut candidate = song_to_candidate(songs.into_iter().next().unwrap(), "/");
        assert_eq!(candidate.album_artist, "Various Artists");
        assert_eq!(candidate.year, "2016");
        assert_eq!(candidate.track, "3");
        assert_eq!(candidate.disc, "");

        let lyric_json = r#"{
            "code": 200,
            "nolyric": false,
            "uncollected": false,
            "lrc": {"version": 10, "lyric": "[00:01.08]考えたってわからないし\n[00:03.06]青空の下、君を待った"},
            "tlyric": {"version": 4, "lyric": "[00:01.08]想过之后依然搞不懂\n[00:03.06]在蔚蓝的天空下等待着你"}
        }"#;

        let lyric_data = parse_lyric_response(lyric_json).expect("lyric JSON parses");
        assert!(!lyric_data.nolyric);
        assert!(!lyric_data.uncollected);
        assert!(lyric_data.raw_lrc.is_some());
        assert!(lyric_data.raw_tlyric.is_some());

        apply_lyrics_to_candidate(&mut candidate, lyric_data);
        assert!(candidate.has_translation);
        assert!(candidate.lyric_preview.contains("考えたってわからないし"));
        assert!(candidate.lyric_preview.contains("想过之后依然搞不懂"));

        let instrumental_lyric = r#"{"code": 200, "nolyric": true, "uncollected": false}"#;
        let inst = parse_lyric_response(instrumental_lyric).expect("instrumental lyric parses");
        assert!(inst.nolyric);
        assert!(inst.raw_lrc.is_none());

        let src = NetEaseSource::new(" / ");
        assert_eq!(src.artist_sep(), " / ");
        assert!(src.search_combo(&SearchQuery::default()).unwrap().is_empty());
        assert!(src.download_bytes("").is_err());
    }

    #[test]
    fn disc_and_album_artist_omit_single_disc_and_redundant_defaults() {
        assert_eq!(normalize_netease_disc(Some(serde_json::json!("01"))), "");
        assert_eq!(normalize_netease_disc(Some(serde_json::json!("1"))), "");
        assert_eq!(normalize_netease_disc(Some(serde_json::json!(1))), "");
        assert_eq!(normalize_netease_disc(Some(serde_json::json!("1/1"))), "");
        assert_eq!(normalize_netease_disc(Some(serde_json::json!("0"))), "");
        assert_eq!(normalize_netease_disc(Some(serde_json::json!("02"))), "2");
        assert_eq!(normalize_netease_disc(Some(serde_json::json!("1/2"))), "1/2");
        assert_eq!(normalize_netease_disc(Some(serde_json::json!("2/2"))), "2/2");

        let mk_song = |cd: Option<&str>, al_artists: Option<Vec<&str>>| RawNetEaseSong {
            id: 1,
            name: "Song".to_string(),
            ar: Some(vec![RawNetEaseArtist {
                id: 10,
                name: "Artist A".to_string(),
            }]),
            artists: None,
            al: Some(RawNetEaseAlbum {
                id: 20,
                name: "Album A".to_string(),
                pic_url: None,
                pic: None,
                pic_str: None,
                publish_time: None,
                artists: al_artists.map(|names| {
                    names
                        .into_iter()
                        .enumerate()
                        .map(|(i, n)| RawNetEaseArtist {
                            id: i as u64 + 100,
                            name: n.to_string(),
                        })
                        .collect()
                }),
                artist: None,
            }),
            album: None,
            dt: Some(180_000),
            duration: None,
            no: Some(1),
            position: None,
            cd: cd.map(|s| serde_json::json!(s)),
            disc: None,
            mv: None,
            mvid: None,
            alia: None,
            alias: None,
            tns: None,
            trans_names: None,
            publish_time: None,
        };

        let cand_absent = song_to_candidate(mk_song(Some("01"), None), ";");
        assert_eq!(cand_absent.album_artist, "");
        assert_eq!(cand_absent.disc, "");

        let cand_same = song_to_candidate(mk_song(Some("1/1"), Some(vec!["artist a"])), ";");
        assert_eq!(cand_same.album_artist, "");
        assert_eq!(cand_same.disc, "");

        let cand_distinct = song_to_candidate(
            mk_song(Some("02"), Some(vec!["Various Artists"])),
            ";",
        );
        assert_eq!(cand_distinct.album_artist, "Various Artists");
        assert_eq!(cand_distinct.disc, "2");

        let cand_multi = song_to_candidate(mk_song(Some("1/2"), Some(vec!["Various Artists"])), ";");
        assert_eq!(cand_multi.disc, "1/2");
    }

    #[test]
    #[ignore = "requires live network access to music.163.com"]
    fn test_netease_live_search_and_lyrics() {
        let source = NetEaseSource::default();
        let detail = source
            .fetch_detail("1357953768")
            .expect("live fetch_detail should succeed for 1357953768");
        assert_eq!(detail.song_id, "1357953768");
        assert!(
            detail.title.contains("だから僕は音楽を辞めた"),
            "unexpected title: {}",
            detail.title
        );

        let lyrics = source
            .fetch_lyrics("1357953768")
            .expect("live fetch_lyrics should succeed");
        let processed = process_netease_lyrics(
            lyrics.raw_lrc.as_deref(),
            lyrics.raw_tlyric.as_deref(),
            lyrics.nolyric,
            &detail.title,
            &LyricProcessOptions::default(),
        );
        match processed {
            ProcessedLyrics::Lyrics(text) => assert!(!text.is_empty()),
            other => panic!("expected processed lyrics, got {other:?}"),
        }

        let meta = detail
            .netease_meta
            .as_ref()
            .expect("netease_meta should be present");
        let key163 = encode_163_key(meta).expect("encode_163_key should succeed");
        assert!(key163.starts_with("163 key(Don't modify):"));
    }
}
