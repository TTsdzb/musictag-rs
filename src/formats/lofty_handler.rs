use std::borrow::Cow;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Cursor, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local};
use filetime::FileTime;
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, GenericImageView, ImageFormat, ImageReader};
use lofty::TextEncoding;
use lofty::config::{ParseOptions, WriteOptions};
use lofty::file::{AudioFile, FileType, TaggedFile, TaggedFileExt};
use lofty::flac::FlacFile;
use lofty::id3::v2::{Frame, FrameId, Id3v2Tag};
use lofty::ogg::{OpusFile, VorbisComments, VorbisFile};
use lofty::picture::{MimeType, Picture, PictureInformation, PictureType};
use lofty::probe::Probe;
use lofty::tag::{ItemKey, ItemValue, Tag, TagExt, TagItem, TagType};

use crate::core::key163::{KEY_163_PREFIX, extract_163_song_id};
use crate::core::lyrics::process_netease_lyrics;
use crate::core::models::{
    AudioTrack, CoverData, CoverWriteSpec, FieldUpdate, LyricProcessOptions, PicFormatLimit,
    ProcessedLyrics, ReplayGainData, TagPatch, WriteOptionsConfig,
};
use crate::core::traits::FormatHandler;

const SUPPORTED_EXTENSIONS: &[&str] = &[
    "flac", "mp3", "m4a", "mp4", "ogg", "opus", "wav", "aiff", "aif", "aifc", "ape", "wv", "mpc",
    "aac",
];

const THUMB_MAX_EDGE: u32 = 256;

const RG_TRACK_GAIN_LOWER: &str = "replaygain_track_gain";
const RG_TRACK_PEAK_LOWER: &str = "replaygain_track_peak";
const RG_ALBUM_GAIN_LOWER: &str = "replaygain_album_gain";
const RG_ALBUM_PEAK_LOWER: &str = "replaygain_album_peak";

#[derive(Debug, Default, Clone, Copy)]
pub struct LoftyFormatHandler;

impl LoftyFormatHandler {
    pub fn new() -> Self {
        Self
    }
}

impl FormatHandler for LoftyFormatHandler {
    fn id(&self) -> &'static str {
        "lofty"
    }

    fn extensions(&self) -> &'static [&'static str] {
        SUPPORTED_EXTENSIONS
    }

    fn read(&self, path: &Path) -> Result<AudioTrack> {
        let tagged_file = open_tagged_file(path)?;
        let file_type = tagged_file.file_type();
        let props = tagged_file.properties();

        let filename = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let filedir = path
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let ext = path
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default();

        let update_time = format_file_mtime(path);
        let tag_types = format_tag_types(path, &tagged_file);

        let mut track = AudioTrack {
            path: path.to_path_buf(),
            filename,
            filedir,
            ext,
            tag_types,
            bit_per_sample: u32::from(props.bit_depth().unwrap_or(0)),
            channels: u32::from(props.channels().unwrap_or(0)),
            sample_rate: props.sample_rate().unwrap_or(0),
            bitrate: props
                .audio_bitrate()
                .or_else(|| props.overall_bitrate())
                .unwrap_or(0),
            duration_ms: props.duration().as_millis() as u64,
            update_time,
            ..Default::default()
        };

        let tag_opt = tagged_file
            .primary_tag()
            .or_else(|| tagged_file.first_tag());

        if let Some(tag) = tag_opt {
            track.title = first_text(tag, &ItemKey::TrackTitle);
            track.artist = join_artists(tag);
            track.album = first_text(tag, &ItemKey::AlbumTitle);
            track.album_artist = join_key_texts(tag, &ItemKey::AlbumArtist, ";");
            track.year = read_year(tag);
            track.track = first_text(tag, &ItemKey::TrackNumber);
            track.disc = first_text(tag, &ItemKey::DiscNumber);
            track.genre = join_key_texts(tag, &ItemKey::Genre, ";");
            track.composer = join_key_texts(tag, &ItemKey::Composer, ";");
            track.lyricist = read_lyricist(tag);

            let (comment, has_163_key, netease_id) = read_comment_and_163(tag, file_type);
            track.comment = comment;
            track.has_163_key = has_163_key;
            track.netease_id = netease_id;

            track.lyrics = read_lyrics(tag);
            track.is_instrumental = detect_is_instrumental(&track.lyrics, &track.title);
            track.replaygain = read_replaygain(tag);
            track.cover = extract_primary_cover_data(tag);
        } else {
            track.is_instrumental = detect_is_instrumental("", &track.title);
        }

        Ok(track)
    }

    fn write_patch(
        &self,
        path: &Path,
        patch: &TagPatch,
        opts: &WriteOptionsConfig,
    ) -> Result<()> {
        if patch.is_empty() {
            return Ok(());
        }

        let orig_mtime = capture_mtime_if_needed(path, opts.keep_mtime)?;
        let mut tagged_file = open_tagged_file(path)?;
        let file_type = tagged_file.file_type();
        let primary_ty = tagged_file.primary_tag_type();

        let vorbis_rg_casing = if matches!(
            file_type,
            FileType::Flac | FileType::Vorbis | FileType::Opus | FileType::Speex
        ) {
            read_vorbis_rg_key_casing(path, file_type)
        } else {
            HashMap::new()
        };

        let target_ty = if let Some(t) = tagged_file.primary_tag() {
            t.tag_type()
        } else if let Some(t) = tagged_file.first_tag() {
            t.tag_type()
        } else {
            tagged_file.insert_tag(Tag::new(primary_ty));
            primary_ty
        };

        let tag = tagged_file
            .tag_mut(target_ty)
            .with_context(|| format!("no tag container available for {}", path.display()))?;

        apply_text_field(tag, &ItemKey::TrackTitle, &patch.title);
        apply_artist_field(tag, &patch.artist, opts.id3v2_version);
        apply_text_field(tag, &ItemKey::AlbumTitle, &patch.album);
        apply_text_field(tag, &ItemKey::AlbumArtist, &patch.album_artist);
        apply_year_field(tag, &patch.year);
        apply_number_pair_field(
            tag,
            &ItemKey::TrackNumber,
            &ItemKey::TrackTotal,
            &patch.track,
        );
        apply_number_pair_field(tag, &ItemKey::DiscNumber, &ItemKey::DiscTotal, &patch.disc);
        apply_text_field(tag, &ItemKey::Genre, &patch.genre);
        apply_text_field(tag, &ItemKey::Composer, &patch.composer);
        apply_lyricist_field(tag, &patch.lyricist);
        apply_comment_field(tag, &patch.comment);
        apply_lyrics_field(tag, &patch.lyrics);
        apply_cover_field(tag, &patch.cover, opts)?;

        if let Some(ref rg) = patch.replaygain {
            apply_replaygain_updates(
                tag,
                rg,
                file_type == FileType::Flac && opts.rg_lowercase_flac,
                &vorbis_rg_casing,
            );
        } else if tag.tag_type() == TagType::VorbisComments && !vorbis_rg_casing.is_empty() {
            restore_vorbis_rg_key_casing(tag, &vorbis_rg_casing);
        }

        save_mutated_tag(path, file_type, tag, opts.id3v2_version == 3)?;

        restore_mtime_if_needed(path, orig_mtime)?;
        Ok(())
    }

    fn write_replaygain(
        &self,
        path: &Path,
        rg: &ReplayGainData,
        opts: &WriteOptionsConfig,
    ) -> Result<()> {
        if rg.is_empty() {
            return Ok(());
        }

        let orig_mtime = capture_mtime_if_needed(path, opts.keep_mtime)?;
        let mut tagged_file = open_tagged_file(path)?;
        let file_type = tagged_file.file_type();
        let primary_ty = tagged_file.primary_tag_type();

        let vorbis_rg_casing = if matches!(
            file_type,
            FileType::Flac | FileType::Vorbis | FileType::Opus | FileType::Speex
        ) {
            read_vorbis_rg_key_casing(path, file_type)
        } else {
            HashMap::new()
        };

        let target_ty = if let Some(t) = tagged_file.primary_tag() {
            t.tag_type()
        } else if let Some(t) = tagged_file.first_tag() {
            t.tag_type()
        } else {
            tagged_file.insert_tag(Tag::new(primary_ty));
            primary_ty
        };

        let tag = tagged_file
            .tag_mut(target_ty)
            .with_context(|| format!("no tag container available for {}", path.display()))?;

        apply_replaygain_updates(
            tag,
            rg,
            file_type == FileType::Flac && opts.rg_lowercase_flac,
            &vorbis_rg_casing,
        );

        save_mutated_tag(path, file_type, tag, opts.id3v2_version == 3)?;

        restore_mtime_if_needed(path, orig_mtime)?;
        Ok(())
    }
}

/// Extracts the primary embedded cover art from `path` to a sidecar image file next to the audio file.
pub fn extract_cover_to_sidecar(path: &Path) -> Result<PathBuf> {
    let probe = Probe::open(path)
        .with_context(|| format!("failed to open {}", path.display()))?
        .options(ParseOptions::new().read_properties(false));
    let tagged_file = match probe.read() {
        Ok(tf) => tf,
        Err(_) => Probe::open(path)?
            .guess_file_type()?
            .options(ParseOptions::new().read_properties(false))
            .read()
            .with_context(|| format!("failed to read tags from {}", path.display()))?,
    };

    let tag = tagged_file
        .primary_tag()
        .or_else(|| tagged_file.first_tag())
        .with_context(|| format!("no tags found in {}", path.display()))?;

    let pic = tag
        .get_picture_type(PictureType::CoverFront)
        .or_else(|| tag.pictures().first())
        .with_context(|| format!("no embedded cover art found in {}", path.display()))?;

    let ext = pic
        .mime_type()
        .and_then(MimeType::ext)
        .map(str::to_string)
        .unwrap_or_else(|| match image::guess_format(pic.data()) {
            Ok(ImageFormat::Png) => "png".to_string(),
            Ok(ImageFormat::WebP) => "webp".to_string(),
            Ok(ImageFormat::Gif) => "gif".to_string(),
            Ok(ImageFormat::Bmp) => "bmp".to_string(),
            _ => "jpg".to_string(),
        });

    let sidecar_path = path.with_extension(ext);
    std::fs::write(&sidecar_path, pic.data())
        .with_context(|| format!("failed to write cover sidecar {}", sidecar_path.display()))?;

    Ok(sidecar_path)
}

/// Processes raw cover art bytes according to resolution, format, and max-size constraints.
pub fn process_cover_image(
    spec: &CoverWriteSpec,
    fallback_res_limit: u32,
    fallback_max_kb: u32,
    fallback_fmt: PicFormatLimit,
) -> Result<(Vec<u8>, MimeType)> {
    if spec.bytes.is_empty() {
        bail!("cover image bytes are empty");
    }

    let target_fmt = if spec.format != PicFormatLimit::Auto {
        spec.format
    } else {
        fallback_fmt
    };
    let max_kb = spec.max_kb.filter(|&k| k > 0).unwrap_or(fallback_max_kb);
    let max_bytes = (max_kb as usize).saturating_mul(1024);

    let orig_format = image::guess_format(&spec.bytes)
        .ok()
        .or_else(|| match spec.mime.to_ascii_lowercase().as_str() {
            "image/png" => Some(ImageFormat::Png),
            "image/jpeg" | "image/jpg" => Some(ImageFormat::Jpeg),
            "image/webp" => Some(ImageFormat::WebP),
            "image/bmp" => Some(ImageFormat::Bmp),
            "image/gif" => Some(ImageFormat::Gif),
            _ => None,
        });

    let decoded = ImageReader::new(Cursor::new(&spec.bytes))
        .with_guessed_format()
        .context("failed to read cover image stream")?
        .decode()
        .context("failed to decode cover image")?;

    let (orig_w, orig_h) = decoded.dimensions();
    let needs_explicit_resize =
        spec.target_width.is_some_and(|w| w > 0) || spec.target_height.is_some_and(|h| h > 0);
    let needs_limit_resize =
        !needs_explicit_resize && fallback_res_limit > 0 && (orig_w > fallback_res_limit || orig_h > fallback_res_limit);
    let size_ok = max_bytes == 0 || spec.bytes.len() <= max_bytes;

    let format_matches = match (target_fmt, orig_format) {
        (PicFormatLimit::Auto, Some(ImageFormat::Jpeg | ImageFormat::Png)) => true,
        (PicFormatLimit::Jpeg, Some(ImageFormat::Jpeg)) => true,
        (PicFormatLimit::Png, Some(ImageFormat::Png)) => true,
        _ => false,
    };

    if !needs_explicit_resize && !needs_limit_resize && size_ok && format_matches {
        let mime = match orig_format {
            Some(ImageFormat::Png) => MimeType::Png,
            _ => MimeType::Jpeg,
        };
        return Ok((spec.bytes.clone(), mime));
    }

    let resized = apply_cover_resize(decoded, spec, fallback_res_limit);

    let prefer_png = match target_fmt {
        PicFormatLimit::Png => true,
        PicFormatLimit::Jpeg => false,
        PicFormatLimit::Auto => orig_format == Some(ImageFormat::Png),
    };

    if prefer_png {
        let mut png_bytes = Vec::new();
        resized
            .write_to(&mut Cursor::new(&mut png_bytes), ImageFormat::Png)
            .context("failed to encode PNG cover")?;

        if max_bytes == 0 || png_bytes.len() <= max_bytes || target_fmt == PicFormatLimit::Png {
            return Ok((png_bytes, MimeType::Png));
        }
    }

    let jpeg_bytes = encode_jpeg_within_limit(resized, max_bytes)?;
    Ok((jpeg_bytes, MimeType::Jpeg))
}

fn apply_cover_resize(
    img: DynamicImage,
    spec: &CoverWriteSpec,
    fallback_res_limit: u32,
) -> DynamicImage {
    let (orig_w, orig_h) = img.dimensions();
    if orig_w == 0 || orig_h == 0 {
        return img;
    }

    match (
        spec.target_width.filter(|&w| w > 0),
        spec.target_height.filter(|&h| h > 0),
    ) {
        (Some(tw), Some(th)) => {
            if tw == orig_w && th == orig_h {
                img
            } else if spec.keep_aspect {
                img.resize(tw, th, FilterType::Lanczos3)
            } else {
                img.resize_exact(tw, th, FilterType::Lanczos3)
            }
        }
        (Some(tw), None) => {
            if tw == orig_w {
                img
            } else {
                let nh = ((u64::from(orig_h) * u64::from(tw)) / u64::from(orig_w)).max(1) as u32;
                img.resize_exact(tw, nh, FilterType::Lanczos3)
            }
        }
        (None, Some(th)) => {
            if th == orig_h {
                img
            } else {
                let nw = ((u64::from(orig_w) * u64::from(th)) / u64::from(orig_h)).max(1) as u32;
                img.resize_exact(nw, th, FilterType::Lanczos3)
            }
        }
        (None, None) => {
            if fallback_res_limit > 0 && (orig_w > fallback_res_limit || orig_h > fallback_res_limit)
            {
                img.resize(fallback_res_limit, fallback_res_limit, FilterType::Lanczos3)
            } else {
                img
            }
        }
    }
}

fn encode_jpeg_within_limit(img: DynamicImage, max_bytes: usize) -> Result<Vec<u8>> {
    const QUALITIES: &[u8] = &[92, 85, 78, 70, 60, 50, 40, 30, 20];

    let mut current = DynamicImage::ImageRgb8(img.to_rgb8());
    let mut best = Vec::new();

    for &q in QUALITIES {
        let mut buf = Vec::new();
        {
            let rgb = current.as_rgb8().expect("converted to rgb8");
            let encoder = JpegEncoder::new_with_quality(&mut buf, q);
            rgb.write_with_encoder(encoder)
                .context("failed to encode JPEG cover")?;
        }
        best = buf;
        if max_bytes == 0 || best.len() <= max_bytes {
            return Ok(best);
        }
    }

    // If even quality 20 exceeds max_bytes, progressively scale down until within budget.
    for _ in 0..8 {
        let (w, h) = current.dimensions();
        if w <= 64 || h <= 64 {
            break;
        }
        let nw = ((w * 4) / 5).max(64);
        let nh = ((h * 4) / 5).max(64);
        current = current.resize_exact(nw, nh, FilterType::Triangle);

        let mut buf = Vec::new();
        {
            let rgb = current.as_rgb8().expect("rgb8 buffer");
            let encoder = JpegEncoder::new_with_quality(&mut buf, 45);
            rgb.write_with_encoder(encoder)
                .context("failed to encode downscaled JPEG cover")?;
        }
        best = buf;
        if best.len() <= max_bytes {
            return Ok(best);
        }
    }

    Ok(best)
}

fn open_tagged_file(path: &Path) -> Result<TaggedFile> {
    match lofty::read_from_path(path) {
        Ok(tf) => Ok(tf),
        Err(first_err) => Probe::open(path)
            .with_context(|| format!("failed to open {}: {first_err}", path.display()))?
            .guess_file_type()
            .with_context(|| format!("failed to probe {}", path.display()))?
            .read()
            .with_context(|| format!("failed to read tags from {}", path.display())),
    }
}

fn format_file_mtime(path: &Path) -> String {
    let Ok(meta) = std::fs::metadata(path) else {
        return String::new();
    };
    let Ok(modified) = meta.modified() else {
        return String::new();
    };
    let dt: DateTime<Local> = modified.into();
    dt.format("%Y-%m-%d %H:%M:%S").to_string()
}

fn capture_mtime_if_needed(path: &Path, keep_mtime: bool) -> Result<Option<FileTime>> {
    if !keep_mtime {
        return Ok(None);
    }
    let meta = std::fs::metadata(path)
        .with_context(|| format!("failed to stat {} for mtime preservation", path.display()))?;
    let modified = meta
        .modified()
        .with_context(|| format!("failed to read mtime for {}", path.display()))?;
    Ok(Some(FileTime::from_system_time(modified)))
}

fn restore_mtime_if_needed(path: &Path, mtime: Option<FileTime>) -> Result<()> {
    if let Some(t) = mtime {
        #[allow(deprecated)]
        filetime::set_file_mtime(path, t)
            .with_context(|| format!("failed to restore mtime on {}", path.display()))?;
    }
    Ok(())
}

fn format_tag_types(path: &Path, tagged_file: &TaggedFile) -> String {
    let file_type = tagged_file.file_type();
    let tags = tagged_file.tags();

    if file_type == FileType::Flac {
        let has_vorbis = tagged_file.contains_tag_type(TagType::VorbisComments);
        let has_id3v2 = tagged_file.contains_tag_type(TagType::Id3v2);
        return match (has_vorbis, has_id3v2) {
            (true, true) => "FLAC (Vorbis, ID3v2)".to_string(),
            (true, false) => "FLAC (Vorbis)".to_string(),
            (false, true) => "FLAC (ID3v2)".to_string(),
            (false, false) => "FLAC".to_string(),
        };
    }

    if tags.is_empty() {
        return match file_type {
            FileType::Aac => "AAC",
            FileType::Aiff => "AIFF",
            FileType::Ape => "APE",
            FileType::Flac => "FLAC",
            FileType::Mpeg => "MP3",
            FileType::Mp4 => "MP4",
            FileType::Mpc => "MPC",
            FileType::Opus => "Opus",
            FileType::Vorbis => "Ogg Vorbis",
            FileType::Speex => "Speex",
            FileType::Wav => "WAV",
            FileType::WavPack => "WavPack",
            _ => "Audio",
        }
        .to_string();
    }

    let mut labels = Vec::with_capacity(tags.len());
    for tag in tags {
        let label = match tag.tag_type() {
            TagType::VorbisComments => match file_type {
                FileType::Opus => "Opus (Vorbis)".to_string(),
                FileType::Vorbis => "Ogg (Vorbis)".to_string(),
                FileType::Speex => "Speex (Vorbis)".to_string(),
                _ => "Vorbis".to_string(),
            },
            TagType::Id3v2 => match detect_id3v2_major_version(path, file_type) {
                Some(2) => "ID3v2.2".to_string(),
                Some(3) => "ID3v2.3".to_string(),
                Some(4) => "ID3v2.4".to_string(),
                _ => "ID3v2".to_string(),
            },
            TagType::Id3v1 => "ID3v1".to_string(),
            TagType::Ape => "APEv2".to_string(),
            TagType::Mp4Ilst => "MP4".to_string(),
            TagType::RiffInfo => "RIFF INFO".to_string(),
            TagType::AiffText => "AIFF Text".to_string(),
            _ => "Tag".to_string(),
        };
        if !labels.contains(&label) {
            labels.push(label);
        }
    }

    labels.join(" · ")
}

fn detect_id3v2_major_version(path: &Path, file_type: FileType) -> Option<u8> {
    let mut file = File::open(path).ok()?;
    match file_type {
        FileType::Wav | FileType::Aiff => {
            let is_be = file_type == FileType::Aiff;
            let mut header = [0u8; 12];
            file.read_exact(&mut header).ok()?;
            file.seek(SeekFrom::Start(12)).ok()?;

            let mut chunk_hdr = [0u8; 8];
            while file.read_exact(&mut chunk_hdr).is_ok() {
                let fourcc = &chunk_hdr[0..4];
                let size_bytes: [u8; 4] = chunk_hdr[4..8].try_into().ok()?;
                let size = if is_be {
                    u32::from_be_bytes(size_bytes)
                } else {
                    u32::from_le_bytes(size_bytes)
                };

                if (fourcc == b"ID3 " || fourcc == b"id3 ") && size >= 10 {
                    let mut id3_hdr = [0u8; 5];
                    file.read_exact(&mut id3_hdr).ok()?;
                    if &id3_hdr[0..3] == b"ID3" {
                        return Some(id3_hdr[3]);
                    }
                    return None;
                }

                let skip = i64::from(size) + i64::from(size % 2);
                if file.seek(SeekFrom::Current(skip)).is_err() {
                    break;
                }
            }
            None
        }
        _ => {
            let mut id3_hdr = [0u8; 5];
            file.read_exact(&mut id3_hdr).ok()?;
            if &id3_hdr[0..3] == b"ID3" {
                Some(id3_hdr[3])
            } else {
                None
            }
        }
    }
}

fn first_text(tag: &Tag, key: &ItemKey) -> String {
    tag.get_string(key).unwrap_or_default().to_string()
}

fn join_key_texts(tag: &Tag, key: &ItemKey, sep: &str) -> String {
    let parts: Vec<&str> = tag
        .get_strings(key)
        .filter(|s| !s.is_empty())
        .collect();
    parts.join(sep)
}

fn join_artists(tag: &Tag) -> String {
    let primary = join_key_texts(tag, &ItemKey::TrackArtist, ";");
    if !primary.is_empty() {
        return primary;
    }
    join_key_texts(tag, &ItemKey::TrackArtists, ";")
}

fn read_year(tag: &Tag) -> String {
    for key in [
        &ItemKey::Year,
        &ItemKey::RecordingDate,
        &ItemKey::ReleaseDate,
        &ItemKey::OriginalReleaseDate,
    ] {
        if let Some(val) = tag.get_string(key) {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    String::new()
}

fn read_lyricist(tag: &Tag) -> String {
    let primary = join_key_texts(tag, &ItemKey::Lyricist, ";");
    if !primary.is_empty() {
        return primary;
    }
    let writer = join_key_texts(tag, &ItemKey::Writer, ";");
    if !writer.is_empty() {
        return writer;
    }
    let unknown: Vec<&str> = tag
        .items()
        .filter_map(|item| match (item.key(), item.value().text()) {
            (ItemKey::Unknown(k), Some(v))
                if !v.is_empty() && k.eq_ignore_ascii_case("LYRICIST") =>
            {
                Some(v)
            }
            _ => None,
        })
        .collect();
    unknown.join(";")
}

fn is_description_item(item: &TagItem) -> bool {
    match item.key() {
        ItemKey::Description => true,
        ItemKey::Unknown(k) => k.eq_ignore_ascii_case("DESCRIPTION"),
        _ => false,
    }
}

fn is_comment_item(item: &TagItem) -> bool {
    match item.key() {
        ItemKey::Comment => true,
        ItemKey::Unknown(k) => k.eq_ignore_ascii_case("COMMENT"),
        _ => false,
    }
}

fn read_comment_and_163(tag: &Tag, file_type: FileType) -> (String, bool, String) {
    let mut comment_candidates: Vec<&TagItem> = Vec::new();
    let mut description_candidates: Vec<&TagItem> = Vec::new();

    for item in tag.items() {
        if is_comment_item(item) {
            comment_candidates.push(item);
        } else if is_description_item(item) {
            description_candidates.push(item);
        }
    }

    // Prefer any COMMENT or DESCRIPTION item that holds a NetEase 163 key.
    for item in comment_candidates
        .iter()
        .chain(description_candidates.iter())
    {
        if let Some(text) = item.value().text() {
            if text.contains(KEY_163_PREFIX) {
                let song_id = extract_163_song_id(text).unwrap_or_default();
                return (text.to_string(), true, song_id);
            }
        }
    }

    // Next, prefer a COMMENT item with an empty description (standard COMM/COMMENT).
    if let Some(text) = comment_candidates
        .iter()
        .filter(|i| i.description().is_empty())
        .find_map(|i| i.value().text())
    {
        return (text.to_string(), false, String::new());
    }

    // Next, prefer a non-iTunes-internal COMMENT item.
    if let Some(text) = comment_candidates
        .iter()
        .filter(|i| !i.description().starts_with("iTun"))
        .find_map(|i| i.value().text())
    {
        return (text.to_string(), false, String::new());
    }

    // On Vorbis/APE containers, fallback to DESCRIPTION if COMMENT is absent.
    if matches!(
        file_type,
        FileType::Flac | FileType::Vorbis | FileType::Opus | FileType::Speex | FileType::Ape
    ) {
        if let Some(text) = description_candidates
            .iter()
            .find_map(|i| i.value().text())
        {
            return (text.to_string(), false, String::new());
        }
    }

    (String::new(), false, String::new())
}

fn read_lyrics(tag: &Tag) -> String {
    if let Some(lyrics) = tag
        .get_items(&ItemKey::Lyrics)
        .filter(|i| i.description().is_empty())
        .find_map(|i| i.value().text())
        .or_else(|| tag.get_string(&ItemKey::Lyrics))
    {
        return lyrics.to_string();
    }

    for key_candidate in ["LYRICS", "UNSYNCEDLYRICS", "UNSYNCED LYRICS"] {
        for item in tag.items() {
            if let ItemKey::Unknown(k) = item.key() {
                let normalized = k.rsplit(':').next().unwrap_or(k);
                if normalized.eq_ignore_ascii_case(key_candidate) {
                    if let Some(text) = item.value().text() {
                        return text.to_string();
                    }
                }
            }
        }
    }

    String::new()
}

fn detect_is_instrumental(lyrics: &str, title: &str) -> bool {
    let opts = LyricProcessOptions {
        skip_instrumental: true,
        ..Default::default()
    };
    matches!(
        process_netease_lyrics(Some(lyrics), None, false, title, &opts),
        ProcessedLyrics::InstrumentalSkipped
    )
}

fn read_replaygain(tag: &Tag) -> ReplayGainData {
    let track_gain = read_rg_field(tag, &ItemKey::ReplayGainTrackGain, RG_TRACK_GAIN_LOWER);
    let track_peak = read_rg_field(tag, &ItemKey::ReplayGainTrackPeak, RG_TRACK_PEAK_LOWER);
    let album_gain = read_rg_field(tag, &ItemKey::ReplayGainAlbumGain, RG_ALBUM_GAIN_LOWER);
    let album_peak = read_rg_field(tag, &ItemKey::ReplayGainAlbumPeak, RG_ALBUM_PEAK_LOWER);

    ReplayGainData {
        track_gain,
        track_peak,
        album_gain,
        album_peak,
    }
}

fn read_rg_field(tag: &Tag, item_key: &ItemKey, lowercase_name: &str) -> Option<String> {
    if let Some(val) = tag
        .get_string(item_key)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return Some(val.to_string());
    }

    for item in tag.items() {
        if let ItemKey::Unknown(k) = item.key() {
            let suffix = k.rsplit(':').next().unwrap_or(k);
            if suffix.eq_ignore_ascii_case(lowercase_name) {
                if let Some(val) = item.value().text().map(str::trim).filter(|s| !s.is_empty()) {
                    return Some(val.to_string());
                }
            }
        }
    }

    None
}

fn extract_primary_cover_data(tag: &Tag) -> Option<CoverData> {
    let pic = tag
        .get_picture_type(PictureType::CoverFront)
        .or_else(|| tag.pictures().first())?;

    let raw_bytes = pic.data().to_vec();
    let size_bytes = raw_bytes.len();
    if raw_bytes.is_empty() {
        return None;
    }

    let mime = pic
        .mime_type()
        .map(|m| m.as_str().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| match image::guess_format(&raw_bytes) {
            Ok(ImageFormat::Png) => "image/png".to_string(),
            Ok(ImageFormat::WebP) => "image/webp".to_string(),
            Ok(ImageFormat::Gif) => "image/gif".to_string(),
            Ok(ImageFormat::Bmp) => "image/bmp".to_string(),
            _ => "image/jpeg".to_string(),
        });

    if let Ok(reader) = ImageReader::new(Cursor::new(&raw_bytes)).with_guessed_format() {
        if let Ok(img) = reader.decode() {
            let (width, height) = img.dimensions();
            let thumb = if width > THUMB_MAX_EDGE || height > THUMB_MAX_EDGE {
                img.thumbnail(THUMB_MAX_EDGE, THUMB_MAX_EDGE)
            } else {
                img
            };
            let rgba = thumb.into_rgba8();
            let thumb_width = rgba.width();
            let thumb_height = rgba.height();
            let rgba_thumb = rgba.into_raw();

            return Some(CoverData {
                bytes: raw_bytes,
                mime,
                width,
                height,
                rgba_thumb,
                thumb_width,
                thumb_height,
                size_bytes,
            });
        }
    }

    let info = PictureInformation::from_picture(pic).unwrap_or_default();
    Some(CoverData {
        bytes: raw_bytes,
        mime,
        width: info.width,
        height: info.height,
        rgba_thumb: Vec::new(),
        thumb_width: 0,
        thumb_height: 0,
        size_bytes,
    })
}

fn apply_text_field(tag: &mut Tag, key: &ItemKey, update: &FieldUpdate<String>) {
    match update {
        FieldUpdate::Keep => {}
        FieldUpdate::Clear => tag.remove_key(key),
        FieldUpdate::Set(v) => {
            tag.remove_key(key);
            if !v.is_empty() {
                tag.insert_text(key.clone(), v.clone());
            }
        }
    }
}

fn apply_artist_field(tag: &mut Tag, update: &FieldUpdate<String>, id3v2_version: u8) {
    match update {
        FieldUpdate::Keep => {}
        FieldUpdate::Clear => {
            tag.remove_key(&ItemKey::TrackArtist);
            tag.remove_key(&ItemKey::TrackArtists);
        }
        FieldUpdate::Set(v) => {
            tag.remove_key(&ItemKey::TrackArtist);
            tag.remove_key(&ItemKey::TrackArtists);
            if v.is_empty() {
                return;
            }
            let supports_multi_item = tag.tag_type() == TagType::VorbisComments
                || (tag.tag_type() == TagType::Id3v2 && id3v2_version != 3);
            if supports_multi_item && v.contains(';') {
                let parts: Vec<&str> = v.split(';').filter(|p| !p.is_empty()).collect();
                if parts.is_empty() {
                    return;
                }
                for part in parts {
                    tag.push(TagItem::new(
                        ItemKey::TrackArtist,
                        ItemValue::Text(part.to_string()),
                    ));
                }
            } else {
                tag.insert_text(ItemKey::TrackArtist, v.clone());
            }
        }
    }
}

fn apply_year_field(tag: &mut Tag, update: &FieldUpdate<String>) {
    match update {
        FieldUpdate::Keep => {}
        FieldUpdate::Clear => {
            tag.remove_key(&ItemKey::Year);
            tag.remove_key(&ItemKey::RecordingDate);
        }
        FieldUpdate::Set(v) => {
            tag.remove_key(&ItemKey::Year);
            tag.remove_key(&ItemKey::RecordingDate);
            let trimmed = v.trim();
            if trimmed.is_empty() {
                return;
            }
            match tag.tag_type() {
                TagType::Ape | TagType::Id3v1 => {
                    tag.insert_text(ItemKey::Year, trimmed.to_string());
                }
                _ => {
                    tag.insert_text(ItemKey::RecordingDate, trimmed.to_string());
                }
            }
        }
    }
}

fn apply_number_pair_field(
    tag: &mut Tag,
    number_key: &ItemKey,
    total_key: &ItemKey,
    update: &FieldUpdate<String>,
) {
    match update {
        FieldUpdate::Keep => {}
        FieldUpdate::Clear => {
            tag.remove_key(number_key);
            tag.remove_key(total_key);
        }
        FieldUpdate::Set(v) => {
            tag.remove_key(number_key);
            tag.remove_key(total_key);
            let trimmed = v.trim();
            if trimmed.is_empty() {
                return;
            }
            if let Some((num_part, total_part)) = trimmed.split_once('/') {
                let n = num_part.trim();
                let t = total_part.trim();
                if !n.is_empty() {
                    tag.insert_text(number_key.clone(), n.to_string());
                }
                if !t.is_empty() {
                    tag.insert_text(total_key.clone(), t.to_string());
                }
            } else {
                tag.insert_text(number_key.clone(), trimmed.to_string());
            }
        }
    }
}

fn apply_lyricist_field(tag: &mut Tag, update: &FieldUpdate<String>) {
    match update {
        FieldUpdate::Keep => {}
        FieldUpdate::Clear => {
            tag.remove_key(&ItemKey::Lyricist);
            tag.retain(|item| !matches!(item.key(), ItemKey::Unknown(k) if k.eq_ignore_ascii_case("LYRICIST")));
        }
        FieldUpdate::Set(v) => {
            tag.remove_key(&ItemKey::Lyricist);
            tag.retain(|item| !matches!(item.key(), ItemKey::Unknown(k) if k.eq_ignore_ascii_case("LYRICIST")));
            if !v.is_empty() {
                tag.insert_text(ItemKey::Lyricist, v.clone());
            }
        }
    }
}

fn should_remove_comment_on_update(item: &TagItem) -> bool {
    if is_comment_item(item) {
        if item.description().is_empty() {
            return true;
        }
        if item
            .value()
            .text()
            .is_some_and(|t| t.contains(KEY_163_PREFIX))
        {
            return true;
        }
        return !item.description().starts_with("iTun");
    }

    if is_description_item(item) {
        return item
            .value()
            .text()
            .is_some_and(|t| t.contains(KEY_163_PREFIX));
    }

    false
}

fn apply_comment_field(tag: &mut Tag, update: &FieldUpdate<String>) {
    match update {
        FieldUpdate::Keep => {}
        FieldUpdate::Clear => {
            tag.retain(|item| !should_remove_comment_on_update(item));
        }
        FieldUpdate::Set(v) => {
            tag.retain(|item| !should_remove_comment_on_update(item));
            if !v.is_empty() {
                tag.push(TagItem::new(ItemKey::Comment, ItemValue::Text(v.clone())));
            }
        }
    }
}

fn is_unknown_lyrics_key(item: &TagItem) -> bool {
    if let ItemKey::Unknown(k) = item.key() {
        let suffix = k.rsplit(':').next().unwrap_or(k);
        return suffix.eq_ignore_ascii_case("LYRICS")
            || suffix.eq_ignore_ascii_case("UNSYNCEDLYRICS")
            || suffix.eq_ignore_ascii_case("UNSYNCED LYRICS");
    }
    false
}

fn apply_lyrics_field(tag: &mut Tag, update: &FieldUpdate<String>) {
    match update {
        FieldUpdate::Keep => {}
        FieldUpdate::Clear => {
            tag.remove_key(&ItemKey::Lyrics);
            tag.retain(|item| !is_unknown_lyrics_key(item));
        }
        FieldUpdate::Set(v) => {
            tag.remove_key(&ItemKey::Lyrics);
            tag.retain(|item| !is_unknown_lyrics_key(item));
            if !v.is_empty() {
                tag.insert_text(ItemKey::Lyrics, v.clone());
            }
        }
    }
}

fn apply_cover_field(
    tag: &mut Tag,
    update: &FieldUpdate<CoverWriteSpec>,
    opts: &WriteOptionsConfig,
) -> Result<()> {
    match update {
        FieldUpdate::Keep => Ok(()),
        FieldUpdate::Clear => {
            while tag.picture_count() > 0 {
                tag.remove_picture(0);
            }
            Ok(())
        }
        FieldUpdate::Set(spec) => {
            let (bytes, mime) = process_cover_image(
                spec,
                opts.pic_res_limit,
                opts.pic_size_limit_kb,
                opts.pic_format,
            )?;
            let picture = Picture::new_unchecked(PictureType::CoverFront, Some(mime), None, bytes);

            if opts.overwrite_cover || spec.overwrite {
                tag.set_picture(0, picture);
                while tag.picture_count() > 1 {
                    tag.remove_picture(1);
                }
            } else if tag.get_picture_type(PictureType::CoverFront).is_none() {
                tag.push_picture(picture);
            }
            Ok(())
        }
    }
}

fn read_vorbis_rg_key_casing(path: &Path, file_type: FileType) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Ok(mut file) = File::open(path) else {
        return map;
    };
    let parse_opts = ParseOptions::new()
        .read_properties(false)
        .read_cover_art(false);

    let collect = |vc: &VorbisComments, map: &mut HashMap<String, String>| {
        for (k, _) in vc.items() {
            let lower = k.to_ascii_lowercase();
            if matches!(
                lower.as_str(),
                RG_TRACK_GAIN_LOWER
                    | RG_TRACK_PEAK_LOWER
                    | RG_ALBUM_GAIN_LOWER
                    | RG_ALBUM_PEAK_LOWER
            ) {
                map.entry(lower).or_insert_with(|| k.to_string());
            }
        }
    };

    match file_type {
        FileType::Flac => {
            if let Ok(flac) = FlacFile::read_from(&mut file, parse_opts) {
                if let Some(vc) = flac.vorbis_comments() {
                    collect(vc, &mut map);
                }
            }
        }
        FileType::Vorbis => {
            if let Ok(vorbis) = VorbisFile::read_from(&mut file, parse_opts) {
                collect(vorbis.vorbis_comments(), &mut map);
            }
        }
        FileType::Opus => {
            if let Ok(opus) = OpusFile::read_from(&mut file, parse_opts) {
                collect(opus.vorbis_comments(), &mut map);
            }
        }
        _ => {}
    }

    map
}

fn restore_vorbis_rg_key_casing(tag: &mut Tag, casing: &HashMap<String, String>) {
    let pairs = [
        (ItemKey::ReplayGainTrackGain, RG_TRACK_GAIN_LOWER),
        (ItemKey::ReplayGainTrackPeak, RG_TRACK_PEAK_LOWER),
        (ItemKey::ReplayGainAlbumGain, RG_ALBUM_GAIN_LOWER),
        (ItemKey::ReplayGainAlbumPeak, RG_ALBUM_PEAK_LOWER),
    ];

    for (item_key, lower_name) in pairs {
        if let Some(orig_key) = casing.get(lower_name) {
            if *orig_key != orig_key.to_ascii_uppercase() {
                let values: Vec<String> = tag
                    .take(&item_key)
                    .filter_map(|i| i.into_value().into_string())
                    .collect();
                for val in values {
                    tag.push_unchecked(TagItem::new(
                        ItemKey::Unknown(orig_key.clone()),
                        ItemValue::Text(val),
                    ));
                }
            }
        }
    }
}

fn apply_replaygain_updates(
    tag: &mut Tag,
    rg: &ReplayGainData,
    lowercase_vorbis: bool,
    vorbis_casing: &HashMap<String, String>,
) {
    let fields: [(&Option<String>, ItemKey, &str); 4] = [
        (&rg.track_gain, ItemKey::ReplayGainTrackGain, RG_TRACK_GAIN_LOWER),
        (&rg.track_peak, ItemKey::ReplayGainTrackPeak, RG_TRACK_PEAK_LOWER),
        (&rg.album_gain, ItemKey::ReplayGainAlbumGain, RG_ALBUM_GAIN_LOWER),
        (&rg.album_peak, ItemKey::ReplayGainAlbumPeak, RG_ALBUM_PEAK_LOWER),
    ];

    // First preserve casing for untouched RG fields if on VorbisComments.
    if tag.tag_type() == TagType::VorbisComments && !vorbis_casing.is_empty() {
        restore_vorbis_rg_key_casing(tag, vorbis_casing);
    }

    for (val_opt, item_key, lower_name) in fields {
        let Some(val) = val_opt.as_deref().map(str::trim).filter(|s| !s.is_empty()) else {
            continue;
        };

        tag.retain(|item| {
            if item.key() == &item_key {
                return false;
            }
            if let ItemKey::Unknown(k) = item.key() {
                let suffix = k.rsplit(':').next().unwrap_or(k);
                if suffix.eq_ignore_ascii_case(lower_name) {
                    return false;
                }
            }
            true
        });

        if tag.tag_type() == TagType::VorbisComments && lowercase_vorbis {
            tag.push_unchecked(TagItem::new(
                ItemKey::Unknown(lower_name.to_string()),
                ItemValue::Text(val.to_string()),
            ));
        } else {
            tag.insert_text(item_key, val.to_string());
        }
    }
}

fn ensure_flac_end_padding(path: &Path) -> Result<()> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("failed to open FLAC {} for padding check", path.display()))?;

    // Skip any leading ID3v2 tag(s) before the "fLaC" stream marker.
    let mut id3_hdr = [0u8; 10];
    loop {
        let pos = file.stream_position()?;
        if file.read_exact(&mut id3_hdr).is_err() {
            return Ok(());
        }
        if &id3_hdr[..3] == b"ID3" {
            let size = (u64::from(id3_hdr[6] & 0x7F) << 21)
                | (u64::from(id3_hdr[7] & 0x7F) << 14)
                | (u64::from(id3_hdr[8] & 0x7F) << 7)
                | u64::from(id3_hdr[9] & 0x7F);
            let footer = if (id3_hdr[5] & 0x10) != 0 { 10 } else { 0 };
            file.seek(SeekFrom::Start(pos + 10 + size + footer))?;
        } else {
            file.seek(SeekFrom::Start(pos))?;
            break;
        }
    }

    let mut magic = [0u8; 4];
    if file.read_exact(&mut magic).is_err() || &magic != b"fLaC" {
        return Ok(());
    }

    loop {
        let hdr_pos = file.stream_position()?;
        let mut hdr = [0u8; 4];
        if file.read_exact(&mut hdr).is_err() {
            return Ok(());
        }
        let is_last = (hdr[0] & 0x80) != 0;
        let block_ty = hdr[0] & 0x7F;
        let block_len = u64::from(u32::from_be_bytes([0, hdr[1], hdr[2], hdr[3]]));
        file.seek(SeekFrom::Current(block_len as i64))?;

        if is_last {
            // lofty 0.22's FLAC writer bugs out when `!end_padding_exists` (corrupting or
            // panicking on offset calculation). Appending an explicit trailing PADDING block
            // keeps lofty on the clean `end_padding_exists == true` path.
            if block_ty == 1 {
                return Ok(());
            }

            let insert_pos = file.stream_position()?;
            let mut tail = Vec::new();
            file.read_to_end(&mut tail)?;

            file.seek(SeekFrom::Start(hdr_pos))?;
            file.write_all(&[hdr[0] & 0x7F])?;

            const PAD_LEN: usize = 1024;
            let mut pad_block = vec![0u8; 4 + PAD_LEN];
            pad_block[0] = 0x81; // last=1, block_type=1 (PADDING)
            pad_block[1..4].copy_from_slice(&(PAD_LEN as u32).to_be_bytes()[1..]);

            file.seek(SeekFrom::Start(insert_pos))?;
            file.write_all(&pad_block)?;
            file.write_all(&tail)?;
            return Ok(());
        }
    }
}

fn save_mutated_tag(
    path: &Path,
    file_type: FileType,
    tag: &Tag,
    is_id3v23: bool,
) -> Result<()> {
    let write_opts = WriteOptions::default().use_id3v23(is_id3v23);
    if tag.tag_type() == TagType::Id3v2 {
        // Convert via Id3v2Tag so Timestamp frames (TDRC -> TYER under ID3v2.3) and
        // COMM/USLT language/description fields are preserved faithfully.
        let mut id3v2: Id3v2Tag = tag.clone().into();
        if is_id3v23 {
            // lofty 0.22 writes a 1-byte NUL when picture description is None even if
            // UTF8 is substituted with UTF16 on ID3v2.3 save, corrupting APIC decoding.
            const APIC_ID: FrameId<'static> = FrameId::Valid(Cow::Borrowed("APIC"));
            let pics: Vec<Frame<'static>> = id3v2.remove(&APIC_ID).collect();
            for frame in pics {
                if let Frame::Picture(mut pic) = frame {
                    if pic
                        .picture
                        .description()
                        .is_none_or(|d| d.chars().all(|c| (c as u32) <= 255))
                    {
                        pic.encoding = TextEncoding::Latin1;
                    }
                    id3v2.insert(Frame::Picture(pic));
                } else {
                    id3v2.insert(frame);
                }
            }
        }
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("failed to open {} for writing", path.display()))?;
        id3v2
            .save_to(&mut file, write_opts)
            .with_context(|| format!("failed to save ID3v2 tag to {}", path.display()))?;
    } else {
        if file_type == FileType::Flac {
            ensure_flac_end_padding(path)?;
        }
        tag.save_to_path(path, write_opts)
            .with_context(|| format!("failed to save tag to {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lofty::config::WriteOptions as LoftyWriteOptions;
    use lofty::iff::wav::WavFile;
    use std::io::Write;
    use tempfile::tempdir;

    fn make_minimal_wav_bytes(sample_rate: u32, num_samples: usize) -> Vec<u8> {
        let channels: u16 = 2;
        let bits_per_sample: u16 = 16;
        let byte_rate = sample_rate * u32::from(channels) * u32::from(bits_per_sample / 8);
        let block_align = channels * (bits_per_sample / 8);
        let data_bytes = (num_samples * usize::from(channels) * 2) as u32;
        let riff_size = 36 + data_bytes;

        let mut buf = Vec::with_capacity(44 + data_bytes as usize);
        buf.extend_from_slice(b"RIFF");
        buf.extend_from_slice(&riff_size.to_le_bytes());
        buf.extend_from_slice(b"WAVE");
        buf.extend_from_slice(b"fmt ");
        buf.extend_from_slice(&16u32.to_le_bytes());
        buf.extend_from_slice(&1u16.to_le_bytes());
        buf.extend_from_slice(&channels.to_le_bytes());
        buf.extend_from_slice(&sample_rate.to_le_bytes());
        buf.extend_from_slice(&byte_rate.to_le_bytes());
        buf.extend_from_slice(&block_align.to_le_bytes());
        buf.extend_from_slice(&bits_per_sample.to_le_bytes());
        buf.extend_from_slice(b"data");
        buf.extend_from_slice(&data_bytes.to_le_bytes());
        buf.resize(44 + data_bytes as usize, 0);
        buf
    }

    fn make_minimal_flac_bytes(sample_rate: u32, total_samples: u64) -> Vec<u8> {
        // Minimal fLaC stream: STREAMINFO (last=true, 34 bytes) + 1 silent FLAC frame?
        // Actually lofty only requires fLaC + STREAMINFO block (34 bytes) for tag read/write!
        let mut buf = Vec::new();
        buf.extend_from_slice(b"fLaC");
        // Block header: last=1 (0x80), type=0 (STREAMINFO), length=34
        buf.push(0x80);
        buf.extend_from_slice(&[0x00, 0x00, 34]);

        // STREAMINFO (34 bytes):
        // min_blocksize=4096, max_blocksize=4096
        buf.extend_from_slice(&4096u16.to_be_bytes());
        buf.extend_from_slice(&4096u16.to_be_bytes());
        // min_framesize=0 (24b), max_framesize=0 (24b)
        buf.extend_from_slice(&[0u8; 6]);
        // sample_rate (20 bits) | channels-1 (3 bits) | bps-1 (5 bits) | total_samples (36 bits)
        // channels=2 -> ch-1 = 1, bps=16 -> bps-1 = 15
        let packed: u64 = (u64::from(sample_rate) << 44)
            | (1u64 << 41)
            | (15u64 << 36)
            | (total_samples & 0x000F_FFFF_FFFF);
        buf.extend_from_slice(&packed.to_be_bytes());
        // MD5 signature (16 bytes)
        buf.extend_from_slice(&[0u8; 16]);
        buf
    }

    fn make_test_png_bytes(width: u32, height: u32) -> Vec<u8> {
        let mut img = image::RgbaImage::new(width, height);
        for (x, y, px) in img.enumerate_pixels_mut() {
            *px = image::Rgba([(x % 255) as u8, (y % 255) as u8, 180, 255]);
        }
        let mut bytes = Vec::new();
        DynamicImage::ImageRgba8(img)
            .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
            .unwrap();
        bytes
    }

    #[test]
    fn wav_id3v2_roundtrip_preserves_replaygain_and_supports_v23_and_v24() {
        let dir = tempdir().unwrap();
        let wav_path = dir.path().join("sample.wav");
        std::fs::write(&wav_path, make_minimal_wav_bytes(44100, 4410)).unwrap();

        let handler = LoftyFormatHandler::new();

        // Write initial ReplayGain + unknown TXXX + cover + tags
        let rg = ReplayGainData {
            track_gain: Some("-8.63 dB".to_string()),
            track_peak: Some("0.987654".to_string()),
            album_gain: Some("-9.10 dB".to_string()),
            album_peak: Some("0.991234".to_string()),
        };
        handler
            .write_replaygain(&wav_path, &rg, &WriteOptionsConfig::default())
            .unwrap();

        let cover_png = make_test_png_bytes(600, 600);
        let patch = TagPatch {
            title: FieldUpdate::Set("だから僕は音楽を辞めた".to_string()),
            artist: FieldUpdate::Set("ヨルシカ".to_string()),
            album: FieldUpdate::Set("エルマ".to_string()),
            album_artist: FieldUpdate::Set("ヨルシカ".to_string()),
            year: FieldUpdate::Set("2019".to_string()),
            track: FieldUpdate::Set("14".to_string()),
            disc: FieldUpdate::Set("1".to_string()),
            genre: FieldUpdate::Set("J-Rock".to_string()),
            composer: FieldUpdate::Set("n-buna".to_string()),
            lyricist: FieldUpdate::Set("n-buna".to_string()),
            comment: FieldUpdate::Set("163 key(Don't modify):abc".to_string()),
            lyrics: FieldUpdate::Set("[00:01.00]考えたってわからないし".to_string()),
            cover: FieldUpdate::Set(CoverWriteSpec {
                bytes: cover_png,
                mime: "image/png".to_string(),
                format: PicFormatLimit::Jpeg,
                max_kb: Some(100),
                target_width: Some(300),
                target_height: Some(300),
                keep_aspect: true,
                overwrite: true,
            }),
            replaygain: None,
        };
        handler
            .write_patch(&wav_path, &patch, &WriteOptionsConfig::default())
            .unwrap();

        let read_back = handler.read(&wav_path).unwrap();
        assert_eq!(read_back.title, "だから僕は音楽を辞めた");
        assert_eq!(read_back.artist, "ヨルシカ");
        assert_eq!(read_back.album, "エルマ");
        assert_eq!(read_back.album_artist, "ヨルシカ");
        assert_eq!(read_back.year, "2019");
        assert_eq!(read_back.track, "14");
        assert_eq!(read_back.disc, "1");
        assert_eq!(read_back.genre, "J-Rock");
        assert_eq!(read_back.composer, "n-buna");
        assert_eq!(read_back.lyricist, "n-buna");
        assert_eq!(read_back.comment, "163 key(Don't modify):abc");
        assert!(read_back.has_163_key);
        assert_eq!(read_back.lyrics, "[00:01.00]考えたってわからないし");
        assert_eq!(read_back.tag_types, "ID3v2.4");
        assert_eq!(read_back.sample_rate, 44100);
        assert_eq!(read_back.channels, 2);
        assert_eq!(read_back.bit_per_sample, 16);

        // ReplayGain MUST be untouched!
        assert_eq!(read_back.replaygain, rg);

        // Cover resized to 300x300 JPEG with <= 256x256 RGBA thumbnail
        let cov = read_back.cover.as_ref().expect("cover present");
        assert_eq!(cov.mime, "image/jpeg");
        assert_eq!((cov.width, cov.height), (300, 300));
        assert!(cov.thumb_width <= 256 && cov.thumb_height <= 256);
        assert_eq!(
            cov.rgba_thumb.len(),
            (cov.thumb_width * cov.thumb_height * 4) as usize
        );

        // Now verify FieldUpdate::Keep leaves fields & ReplayGain intact while updating only title in ID3v2.3
        let past = FileTime::from_unix_time(1_700_000_000, 0);
        #[allow(deprecated)]
        filetime::set_file_mtime(&wav_path, past).unwrap();

        let v23_opts = WriteOptionsConfig {
            id3v2_version: 3,
            keep_mtime: true,
            ..Default::default()
        };
        let patch_title_only = TagPatch {
            title: FieldUpdate::Set("Updated Title".to_string()),
            genre: FieldUpdate::Clear,
            ..Default::default()
        };
        handler
            .write_patch(&wav_path, &patch_title_only, &v23_opts)
            .unwrap();

        let after_mtime = FileTime::from_system_time(std::fs::metadata(&wav_path).unwrap().modified().unwrap());
        assert_eq!(after_mtime, past);

        let read_v23 = handler.read(&wav_path).unwrap();
        assert_eq!(read_v23.tag_types, "ID3v2.3");
        assert_eq!(read_v23.title, "Updated Title");
        assert_eq!(read_v23.genre, "");
        assert_eq!(read_v23.year, "2019");
        assert_eq!(read_v23.artist, "ヨルシカ");
        assert_eq!(read_v23.replaygain, rg);

        // Extract cover to sidecar
        let sidecar = extract_cover_to_sidecar(&wav_path).unwrap();
        assert!(sidecar.exists());
        assert_eq!(sidecar.extension().and_then(|s| s.to_str()), Some("jpg"));
    }

    #[test]
    fn flac_vorbis_163_description_and_lowercase_replaygain_roundtrip() {
        let dir = tempdir().unwrap();

        // First verify writing directly to a padding-less FLAC preserves trailing audio bytes.
        let no_pad_path = dir.path().join("no_pad.flac");
        let audio_tail = [0xFFu8, 0xF8, 0x59, 0x10, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88];
        let mut no_pad_bytes = make_minimal_flac_bytes(44100, 44100 * 10);
        no_pad_bytes.extend_from_slice(&audio_tail);
        std::fs::write(&no_pad_path, &no_pad_bytes).unwrap();
        let handler = LoftyFormatHandler::new();
        handler
            .write_patch(
                &no_pad_path,
                &TagPatch {
                    title: FieldUpdate::Set("No-Pad Title".to_string()),
                    ..Default::default()
                },
                &WriteOptionsConfig::default(),
            )
            .unwrap();
        let read_no_pad = handler.read(&no_pad_path).unwrap();
        assert_eq!(read_no_pad.title, "No-Pad Title");
        let saved_no_pad = std::fs::read(&no_pad_path).unwrap();
        assert!(saved_no_pad.ends_with(&audio_tail));

        let flac_path = dir.path().join("track.flac");
        std::fs::write(&flac_path, make_minimal_flac_bytes(44100, 44100 * 10)).unwrap();
        ensure_flac_end_padding(&flac_path).unwrap();

        // Seed raw VorbisComments with DESCRIPTION containing 163 key, non-163 DESCRIPTION,
        // multiple ARTISTs, UNSYNCEDLYRICS, and mixed-case ReplayGain.
        let payload = crate::core::key163::NetEase163Payload {
            format: "flac".to_string(),
            music_id: 1357953768,
            music_name: "だから僕は音楽を辞めた".to_string(),
            artist: vec![("ヨルシカ".to_string(), 28082342)],
            album: "だから僕は音楽を辞めた".to_string(),
            ..Default::default()
        };
        let key_str = crate::core::key163::encode_163_key(&payload).unwrap();

        {
            let mut vc = VorbisComments::new();
            vc.push("TITLE".to_string(), "Song".to_string());
            vc.push("ARTIST".to_string(), "Artist 1".to_string());
            vc.push("ARTIST".to_string(), "Artist 2".to_string());
            vc.push("DESCRIPTION".to_string(), key_str.clone());
            vc.push("DESCRIPTION".to_string(), "Keep me untouched".to_string());
            vc.push("UNSYNCEDLYRICS".to_string(), "[00:01.00]Line 1".to_string());
            vc.push("REPLAYGAIN_TRACK_GAIN".to_string(), "-5.00 dB".to_string());
            vc.push("CUSTOM_TAG".to_string(), "preserve-me".to_string());
            vc.save_to_path(&flac_path, LoftyWriteOptions::default())
                .unwrap();
        }

        let handler = LoftyFormatHandler::new();
        let track = handler.read(&flac_path).unwrap();
        assert_eq!(track.tag_types, "FLAC (Vorbis)");
        assert_eq!(track.artist, "Artist 1;Artist 2");
        assert_eq!(track.comment, key_str);
        assert!(track.has_163_key);
        assert_eq!(track.netease_id, "1357953768");
        assert_eq!(track.lyrics, "[00:01.00]Line 1");
        assert_eq!(track.replaygain.track_gain.as_deref(), Some("-5.00 dB"));

        // Write lowercase ReplayGain on FLAC
        let rg = ReplayGainData {
            track_gain: Some("-8.12 dB".to_string()),
            track_peak: Some("0.990000".to_string()),
            album_gain: Some("-8.50 dB".to_string()),
            album_peak: Some("0.995000".to_string()),
        };
        handler
            .write_replaygain(
                &flac_path,
                &rg,
                &WriteOptionsConfig {
                    rg_lowercase_flac: true,
                    ..Default::default()
                },
            )
            .unwrap();

        // Inspect raw VorbisComments keys for exact lowercase casing!
        {
            let mut f = File::open(&flac_path).unwrap();
            let flac = FlacFile::read_from(&mut f, ParseOptions::new()).unwrap();
            let vc = flac.vorbis_comments().unwrap();
            let keys: Vec<(&str, &str)> = vc.items().collect();
            assert!(keys.contains(&("replaygain_track_gain", "-8.12 dB")));
            assert!(keys.contains(&("replaygain_track_peak", "0.990000")));
            assert!(keys.contains(&("replaygain_album_gain", "-8.50 dB")));
            assert!(keys.contains(&("replaygain_album_peak", "0.995000")));
            assert!(!keys.iter().any(|(k, _)| *k == "REPLAYGAIN_TRACK_GAIN"));
            assert!(keys.contains(&("CUSTOM_TAG", "preserve-me")));
        }

        // Update comment -> writes COMMENT, removes stale 163 DESCRIPTION, keeps non-163 DESCRIPTION & lowercase RG!
        let new_comment = "New comment".to_string();
        let patch = TagPatch {
            comment: FieldUpdate::Set(new_comment.clone()),
            ..Default::default()
        };
        handler
            .write_patch(&flac_path, &patch, &WriteOptionsConfig::default())
            .unwrap();

        {
            let mut f = File::open(&flac_path).unwrap();
            let flac = FlacFile::read_from(&mut f, ParseOptions::new()).unwrap();
            let vc = flac.vorbis_comments().unwrap();
            let items: Vec<(&str, &str)> = vc.items().collect();
            assert!(items.contains(&("COMMENT", "New comment")));
            assert!(items.contains(&("DESCRIPTION", "Keep me untouched")));
            assert!(!items.iter().any(|(k, v)| k.eq_ignore_ascii_case("DESCRIPTION") && v.contains(KEY_163_PREFIX)));
            // Existing lowercase replaygain keys stayed lowercase!
            assert!(items.contains(&("replaygain_track_gain", "-8.12 dB")));
            assert!(items.contains(&("CUSTOM_TAG", "preserve-me")));
        }

        let updated = handler.read(&flac_path).unwrap();
        assert_eq!(updated.comment, "New comment");
        assert!(!updated.has_163_key);
        assert_eq!(updated.replaygain, rg);

        // Also test upper-case FLAC ReplayGain flag (rg_lowercase_flac = false)
        handler
            .write_replaygain(
                &flac_path,
                &ReplayGainData {
                    track_gain: Some("-7.00 dB".to_string()),
                    track_peak: Some("0.880000".to_string()),
                    ..Default::default()
                },
                &WriteOptionsConfig {
                    rg_lowercase_flac: false,
                    ..Default::default()
                },
            )
            .unwrap();
        {
            let mut f = File::open(&flac_path).unwrap();
            let flac = FlacFile::read_from(&mut f, ParseOptions::new()).unwrap();
            let vc = flac.vorbis_comments().unwrap();
            let items: Vec<(&str, &str)> = vc.items().collect();
            assert!(items.contains(&("REPLAYGAIN_TRACK_GAIN", "-7.00 dB")));
            assert!(items.contains(&("REPLAYGAIN_TRACK_PEAK", "0.880000")));
        }
    }

    #[test]
    fn instrumental_detection_and_cover_compression_work() {
        let png = make_test_png_bytes(1000, 800);
        let spec = CoverWriteSpec {
            bytes: png,
            mime: "image/png".to_string(),
            target_width: Some(500),
            target_height: Some(500),
            keep_aspect: true,
            max_kb: Some(40),
            format: PicFormatLimit::Jpeg,
            overwrite: true,
        };
        let (out, mime) = process_cover_image(&spec, 0, 2000, PicFormatLimit::Auto).unwrap();
        assert_eq!(mime, MimeType::Jpeg);
        assert!(out.len() <= 40 * 1024);

        let decoded = ImageReader::new(Cursor::new(&out))
            .with_guessed_format()
            .unwrap()
            .decode()
            .unwrap();
        assert_eq!(decoded.dimensions(), (500, 400));

        assert!(detect_is_instrumental("[00:00.00]纯音乐，请欣赏", "Track"));
        assert!(detect_is_instrumental("", "Song (Instrumental)"));
        assert!(!detect_is_instrumental("[00:01.00]Normal lyrics", "Normal Song"));

        // Verify WavFile compiles & parses cleanly if used with lofty directly
        let dir = tempdir().unwrap();
        let wav_path = dir.path().join("empty.wav");
        let mut f = File::create(&wav_path).unwrap();
        f.write_all(&make_minimal_wav_bytes(44100, 100)).unwrap();
        let mut r = File::open(&wav_path).unwrap();
        assert!(WavFile::read_from(&mut r, ParseOptions::new()).is_ok());

        // Verify overwrite_cover = false preserves existing cover and Clear removes cover/lyrics
        let handler = LoftyFormatHandler::new();
        let red_png = make_test_png_bytes(120, 120);
        let blue_png = make_test_png_bytes(200, 200);
        handler
            .write_patch(
                &wav_path,
                &TagPatch {
                    cover: FieldUpdate::Set(CoverWriteSpec {
                        bytes: red_png,
                        mime: "image/png".to_string(),
                        ..Default::default()
                    }),
                    lyrics: FieldUpdate::Set("[00:00.50]Hello".to_string()),
                    ..Default::default()
                },
                &WriteOptionsConfig::default(),
            )
            .unwrap();

        handler
            .write_patch(
                &wav_path,
                &TagPatch {
                    cover: FieldUpdate::Set(CoverWriteSpec {
                        bytes: blue_png,
                        mime: "image/png".to_string(),
                        overwrite: false,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                &WriteOptionsConfig {
                    overwrite_cover: false,
                    ..Default::default()
                },
            )
            .unwrap();
        let preserved = handler.read(&wav_path).unwrap();
        assert_eq!(preserved.cover.as_ref().unwrap().width, 120);

        handler
            .write_patch(
                &wav_path,
                &TagPatch {
                    cover: FieldUpdate::Clear,
                    lyrics: FieldUpdate::Clear,
                    ..Default::default()
                },
                &WriteOptionsConfig::default(),
            )
            .unwrap();
        let cleared = handler.read(&wav_path).unwrap();
        assert!(cleared.cover.is_none());
        assert_eq!(cleared.lyrics, "");
    }
}
