use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::core::key163::NetEase163Payload;

/// Tri-state field update used when editing single or multiple tracks.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum FieldUpdate<T> {
    /// Leave the existing value untouched.
    #[default]
    Keep,
    /// Replace the field with the provided value.
    Set(T),
    /// Clear/remove the field when saving.
    Clear,
}

impl<T> FieldUpdate<T> {
    pub fn is_keep(&self) -> bool {
        matches!(self, Self::Keep)
    }

    pub fn is_set(&self) -> bool {
        matches!(self, Self::Set(_))
    }

    pub fn is_clear(&self) -> bool {
        matches!(self, Self::Clear)
    }

    pub fn as_ref(&self) -> FieldUpdate<&T> {
        match self {
            Self::Keep => FieldUpdate::Keep,
            Self::Set(v) => FieldUpdate::Set(v),
            Self::Clear => FieldUpdate::Clear,
        }
    }

    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> FieldUpdate<U> {
        match self {
            Self::Keep => FieldUpdate::Keep,
            Self::Set(v) => FieldUpdate::Set(f(v)),
            Self::Clear => FieldUpdate::Clear,
        }
    }
}

/// Patch describing tag changes to write to an audio file.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TagPatch {
    pub title: FieldUpdate<String>,
    pub artist: FieldUpdate<String>,
    pub album: FieldUpdate<String>,
    pub album_artist: FieldUpdate<String>,
    pub year: FieldUpdate<String>,
    pub track: FieldUpdate<String>,
    pub disc: FieldUpdate<String>,
    pub genre: FieldUpdate<String>,
    pub composer: FieldUpdate<String>,
    pub lyricist: FieldUpdate<String>,
    pub comment: FieldUpdate<String>,
    pub cover: FieldUpdate<CoverWriteSpec>,
    pub lyrics: FieldUpdate<String>,
    pub replaygain: Option<ReplayGainData>,
}

impl TagPatch {
    /// Returns true when no field, cover, lyrics, or ReplayGain update is requested.
    pub fn is_empty(&self) -> bool {
        self.title.is_keep()
            && self.artist.is_keep()
            && self.album.is_keep()
            && self.album_artist.is_keep()
            && self.year.is_keep()
            && self.track.is_keep()
            && self.disc.is_keep()
            && self.genre.is_keep()
            && self.composer.is_keep()
            && self.lyricist.is_keep()
            && self.comment.is_keep()
            && self.cover.is_keep()
            && self.lyrics.is_keep()
            && self.replaygain.is_none()
    }
}

/// Output image format constraint when embedding album cover art.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PicFormatLimit {
    #[default]
    Auto,
    Jpeg,
    Png,
}

impl PicFormatLimit {
    pub fn from_index(idx: i32) -> Self {
        match idx {
            1 => Self::Jpeg,
            2 => Self::Png,
            _ => Self::Auto,
        }
    }

    pub fn to_index(self) -> i32 {
        match self {
            Self::Auto => 0,
            Self::Jpeg => 1,
            Self::Png => 2,
        }
    }
}

/// Specification for writing or resizing an embedded cover image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoverWriteSpec {
    pub bytes: Vec<u8>,
    pub mime: String,
    pub target_width: Option<u32>,
    pub target_height: Option<u32>,
    pub keep_aspect: bool,
    pub max_kb: Option<u32>,
    pub format: PicFormatLimit,
    pub overwrite: bool,
}

impl Default for CoverWriteSpec {
    fn default() -> Self {
        Self {
            bytes: Vec::new(),
            mime: "image/jpeg".to_string(),
            target_width: None,
            target_height: None,
            keep_aspect: true,
            max_kb: None,
            format: PicFormatLimit::Auto,
            overwrite: true,
        }
    }
}

/// Decoded cover art payload and thumbnail stored on a loaded track.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CoverData {
    pub bytes: Vec<u8>,
    pub mime: String,
    pub width: u32,
    pub height: u32,
    pub rgba_thumb: Vec<u8>,
    pub thumb_width: u32,
    pub thumb_height: u32,
    pub size_bytes: usize,
}

impl CoverData {
    /// Formats a human-readable resolution/codec/size summary (e.g. `1400×1400 · JPEG · 680 KB`).
    pub fn format_info(&self) -> String {
        let fmt = if self.mime.eq_ignore_ascii_case("image/png") {
            "PNG"
        } else if self.mime.eq_ignore_ascii_case("image/webp") {
            "WEBP"
        } else if self.mime.eq_ignore_ascii_case("image/gif") {
            "GIF"
        } else if self.mime.eq_ignore_ascii_case("image/bmp") {
            "BMP"
        } else {
            "JPEG"
        };
        let size_str = if self.size_bytes >= 1024 * 1024 {
            format!("{:.2} MB", self.size_bytes as f64 / (1024.0 * 1024.0))
        } else {
            format!("{} KB", (self.size_bytes + 1023) / 1024)
        };
        format!("{}×{} · {} · {}", self.width, self.height, fmt, size_str)
    }
}

/// ReplayGain metadata fields (standard track & album gain/peak strings).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ReplayGainData {
    pub track_gain: Option<String>,
    pub track_peak: Option<String>,
    pub album_gain: Option<String>,
    pub album_peak: Option<String>,
}

impl ReplayGainData {
    pub fn is_empty(&self) -> bool {
        self.track_gain.as_deref().unwrap_or("").is_empty()
            && self.track_peak.as_deref().unwrap_or("").is_empty()
            && self.album_gain.as_deref().unwrap_or("").is_empty()
            && self.album_peak.as_deref().unwrap_or("").is_empty()
    }
}

/// Complete metadata and audio properties of a local audio file.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AudioTrack {
    pub path: PathBuf,
    pub filename: String,
    pub filedir: String,
    pub ext: String,
    pub tag_types: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub album_artist: String,
    pub year: String,
    pub track: String,
    pub disc: String,
    pub genre: String,
    pub composer: String,
    pub lyricist: String,
    pub comment: String,
    pub has_163_key: bool,
    pub netease_id: String,
    pub cover: Option<CoverData>,
    pub lyrics: String,
    pub is_instrumental: bool,
    pub replaygain: ReplayGainData,
    pub bit_per_sample: u32,
    pub channels: u32,
    pub sample_rate: u32,
    pub bitrate: u32,
    pub duration_ms: u64,
    pub update_time: String,
}

impl AudioTrack {
    /// Formats `duration_ms` as `MM:SS`.
    pub fn duration_formatted(&self) -> String {
        let total_secs = self.duration_ms / 1000;
        let mins = total_secs / 60;
        let secs = total_secs % 60;
        format!("{mins:02}:{secs:02}")
    }
}

/// Options passed to `FormatHandler` when writing tags or ReplayGain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteOptionsConfig {
    /// ID3v2 version for MP3/ID3 containers (`3` for ID3v2.3, `4` for ID3v2.4).
    pub id3v2_version: u8,
    pub keep_mtime: bool,
    pub overwrite_cover: bool,
    pub pic_res_limit: u32,
    pub pic_size_limit_kb: u32,
    pub pic_format: PicFormatLimit,
    pub rg_lowercase_flac: bool,
}

impl Default for WriteOptionsConfig {
    fn default() -> Self {
        Self {
            id3v2_version: 4,
            keep_mtime: false,
            overwrite_cover: true,
            pic_res_limit: 0,
            pic_size_limit_kb: 2000,
            pic_format: PicFormatLimit::Auto,
            rg_lowercase_flac: true,
        }
    }
}

/// Capabilities declared by a tag/lyric/cover source plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SourceCapabilities {
    pub combo: bool,
    pub lyrics: bool,
    pub cover: bool,
    pub key163: bool,
}

/// Descriptive metadata for a registered `TagSource`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SourceInfo {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub builtin: bool,
    pub enabled: bool,
    pub capabilities: SourceCapabilities,
}

/// Query parameters sent to online `TagSource` backends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchQuery {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub song_id: Option<String>,
    pub limit: usize,
}

impl Default for SearchQuery {
    fn default() -> Self {
        Self {
            title: String::new(),
            artist: String::new(),
            album: String::new(),
            song_id: None,
            limit: 10,
        }
    }
}

/// Candidate track returned by a `TagSource` search or detail lookup.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CandidateTrack {
    pub source_id: String,
    pub source_name: String,
    pub song_id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub album_artist: String,
    pub year: String,
    pub track: String,
    pub disc: String,
    pub genre: String,
    pub composer: String,
    pub lyricist: String,
    pub comment: String,
    pub duration_ms: u64,
    pub cover_url: Option<String>,
    pub cover_size: String,
    pub cover_bytes: Option<Vec<u8>>,
    pub netease_meta: Option<NetEase163Payload>,
    pub raw_lrc: Option<String>,
    pub raw_tlyric: Option<String>,
    pub nolyric: bool,
    pub is_instrumental: bool,
    pub has_translation: bool,
    pub lyric_preview: String,
}

/// Raw lyric payload returned by a `TagSource`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LyricData {
    pub raw_lrc: Option<String>,
    pub raw_tlyric: Option<String>,
    pub nolyric: bool,
    pub uncollected: bool,
}

/// Cover search candidate returned by a `TagSource`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CoverSearchItem {
    pub source_id: String,
    pub source_name: String,
    pub title: String,
    pub url: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub size_bytes: Option<usize>,
    pub format: String,
    pub thumb_bytes: Option<Vec<u8>>,
}

/// Chinese script conversion mode for lyrics and batch text operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ZhConvMode {
    #[default]
    None,
    ToSimplified,
    ToTraditional,
}

impl ZhConvMode {
    pub fn from_index(idx: i32) -> Self {
        match idx {
            1 => Self::ToSimplified,
            2 => Self::ToTraditional,
            _ => Self::None,
        }
    }

    pub fn to_index(self) -> i32 {
        match self {
            Self::None => 0,
            Self::ToSimplified => 1,
            Self::ToTraditional => 2,
        }
    }
}

/// Options controlling lyric downloading, bilingual alignment, and cleanup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LyricProcessOptions {
    pub dl_trans: bool,
    pub dont_dl_orig: bool,
    /// Bilingual layout:
    /// - `0`: `[MM:SS.xx]orig\r\n[MM:SS.xx]trans`
    /// - `1`: `[MM:SS.xx]orig<conn>trans`
    /// - `2`: interleaved `orig` then `trans` at original timestamps
    /// - `3`: `[MM:SS.xx]trans\r\n[MM:SS.xx]orig`
    pub trans_format: u8,
    pub trans_connector: String,
    pub delete_head_tag: bool,
    pub delete_blank_lines: bool,
    pub reformat_timetag: bool,
    pub remove_timetag: bool,
    pub skip_instrumental: bool,
    pub zh_conv: ZhConvMode,
}

impl Default for LyricProcessOptions {
    fn default() -> Self {
        Self {
            dl_trans: true,
            dont_dl_orig: false,
            trans_format: 0,
            trans_connector: "\\n".to_string(),
            delete_head_tag: true,
            delete_blank_lines: true,
            reformat_timetag: true,
            remove_timetag: false,
            skip_instrumental: true,
            zh_conv: ZhConvMode::None,
        }
    }
}

/// Result of processing raw lyric payloads with `LyricProcessOptions`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProcessedLyrics {
    Lyrics(String),
    InstrumentalSkipped,
    Empty,
}

/// ReplayGain scan mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum RgScanMode {
    #[default]
    TrackOnly,
    AlbumAndTrack,
}

impl RgScanMode {
    pub fn from_index(idx: i32) -> Self {
        match idx {
            1 => Self::AlbumAndTrack,
            _ => Self::TrackOnly,
        }
    }

    pub fn to_index(self) -> i32 {
        match self {
            Self::TrackOnly => 0,
            Self::AlbumAndTrack => 1,
        }
    }
}
