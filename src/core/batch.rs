use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use encoding_rs::{Encoding, WINDOWS_1252};
use regex::Regex;

use crate::core::lyrics::convert_zh;
use crate::core::models::{AudioTrack, FieldUpdate, TagPatch, ZhConvMode};

pub const PRESET_TEMPLATES: &[&str] = &[
    "%artist% - %title%",
    "%track%. %title%",
    "%title% - %artist%",
    "%track% - %artist% - %title%",
];

pub const SUPPORTED_CHARSETS: &[&str] = &[
    "GBK",
    "GB18030",
    "Big5",
    "Shift_JIS",
    "ISO-8859-1",
    "UTF-8",
];

const SIDECAR_EXTENSIONS: &[&str] = &["lrc", "jpg", "jpeg", "png"];

/// Tag field targeted by template placeholders (`%...%`, `@0..@8`) or regex capture groups.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TemplateField {
    Ignore,
    Title,
    Artist,
    Album,
    Disc,
    Track,
    Year,
    Comment,
    AlbumArtist,
    Genre,
    Composer,
    Lyricist,
}

impl TemplateField {
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Ignore => "无用途",
            Self::Title => "标题",
            Self::Artist => "艺术家",
            Self::Album => "专辑",
            Self::Disc => "碟号",
            Self::Track => "音轨号",
            Self::Year => "年份",
            Self::Comment => "注释",
            Self::AlbumArtist => "专辑艺术家",
            Self::Genre => "流派",
            Self::Composer => "作曲家",
            Self::Lyricist => "作词家",
        }
    }

    pub fn from_positional_digit(digit: char) -> Option<Self> {
        match digit {
            '0' => Some(Self::Ignore),
            '1' => Some(Self::Title),
            '2' => Some(Self::Artist),
            '3' => Some(Self::Album),
            '4' => Some(Self::Disc),
            '5' => Some(Self::Track),
            '6' => Some(Self::Year),
            '7' => Some(Self::Comment),
            '8' => Some(Self::AlbumArtist),
            _ => None,
        }
    }

    pub fn from_token(token: &str) -> Option<Self> {
        let trimmed = token.trim();
        if let Some(rest) = trimmed.strip_prefix('@') {
            if let Some(first) = rest.chars().next() {
                if let Some(field) = Self::from_positional_digit(first) {
                    return Some(field);
                }
            }
        }
        let lower = trimmed.trim_matches('%').to_ascii_lowercase();
        match lower.as_str() {
            "ignore" | "unused" | "无用途" => Some(Self::Ignore),
            "title" | "标题" => Some(Self::Title),
            "artist" | "艺术家" => Some(Self::Artist),
            "album" | "专辑" => Some(Self::Album),
            "disc" | "discnumber" | "碟号" => Some(Self::Disc),
            "track" | "tracknumber" | "音轨号" | "音轨" => Some(Self::Track),
            "year" | "date" | "年份" => Some(Self::Year),
            "comment" | "注释" => Some(Self::Comment),
            "albumartist" | "album_artist" | "album-artist" | "专辑艺术家" => {
                Some(Self::AlbumArtist)
            }
            "genre" | "流派" | "风格" => Some(Self::Genre),
            "composer" | "作曲" | "作曲家" => Some(Self::Composer),
            "lyricist" | "作词" | "作词家" => Some(Self::Lyricist),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TemplateSegment {
    Literal(String),
    Field(TemplateField),
}

/// Resolves preset pattern index `0..=3` or falls back to `custom_pattern`.
pub fn resolve_template(pattern_index: usize, custom_pattern: &str) -> &str {
    PRESET_TEMPLATES
        .get(pattern_index)
        .copied()
        .unwrap_or(custom_pattern)
}

fn parse_template(template: &str) -> Vec<TemplateSegment> {
    let mut segments = Vec::new();
    let mut literal_buf = String::new();
    let chars: Vec<char> = template.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        if chars[i] == '@' && i + 1 < chars.len() {
            if let Some(field) = TemplateField::from_positional_digit(chars[i + 1]) {
                if !literal_buf.is_empty() {
                    segments.push(TemplateSegment::Literal(std::mem::take(&mut literal_buf)));
                }
                segments.push(TemplateSegment::Field(field));
                i += 2;
                continue;
            }
        }

        if chars[i] == '%' {
            if let Some(rel_end) = chars[i + 1..].iter().position(|&c| c == '%') {
                let inner: String = chars[i + 1..i + 1 + rel_end].iter().collect();
                if let Some(field) = TemplateField::from_token(&inner) {
                    if !literal_buf.is_empty() {
                        segments.push(TemplateSegment::Literal(std::mem::take(&mut literal_buf)));
                    }
                    segments.push(TemplateSegment::Field(field));
                    i += 1 + rel_end + 1;
                    continue;
                }
            }
        }

        literal_buf.push(chars[i]);
        i += 1;
    }

    if !literal_buf.is_empty() {
        segments.push(TemplateSegment::Literal(literal_buf));
    }

    segments
}

/// Extracts filename stem without stripping `.` inside titles like `03. Artist - Title`.
pub fn extract_filename_stem(filename_or_path: &str) -> String {
    let name = Path::new(filename_or_path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(filename_or_path);

    if let Some(dot) = name.rfind('.') {
        let ext = &name[dot + 1..];
        if !ext.is_empty() && ext.len() <= 6 && ext.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return name[..dot].to_string();
        }
    }
    name.to_string()
}

fn set_patch_field(patch: &mut TagPatch, field: TemplateField, value: String) {
    match field {
        TemplateField::Ignore => {}
        TemplateField::Title => patch.title = FieldUpdate::Set(value),
        TemplateField::Artist => patch.artist = FieldUpdate::Set(value),
        TemplateField::Album => patch.album = FieldUpdate::Set(value),
        TemplateField::Disc => patch.disc = FieldUpdate::Set(value),
        TemplateField::Track => patch.track = FieldUpdate::Set(value),
        TemplateField::Year => patch.year = FieldUpdate::Set(value),
        TemplateField::Comment => patch.comment = FieldUpdate::Set(value),
        TemplateField::AlbumArtist => patch.album_artist = FieldUpdate::Set(value),
        TemplateField::Genre => patch.genre = FieldUpdate::Set(value),
        TemplateField::Composer => patch.composer = FieldUpdate::Set(value),
        TemplateField::Lyricist => patch.lyricist = FieldUpdate::Set(value),
    }
}

/// Parses tags from a filename or path using a template (`%title%`, `@1`, ...).
pub fn filename_to_tags(filename_or_path: &str, template: &str) -> Result<TagPatch> {
    let segments = parse_template(template);
    let fields: Vec<TemplateField> = segments
        .iter()
        .filter_map(|s| match s {
            TemplateSegment::Field(f) => Some(*f),
            TemplateSegment::Literal(_) => None,
        })
        .collect();
    if fields.is_empty() {
        bail!("template contains no tag placeholders: {template}");
    }

    let mut regex_str = String::from("^");
    let mut field_positions = Vec::new();
    let total_segments = segments.len();

    for (idx, seg) in segments.iter().enumerate() {
        match seg {
            TemplateSegment::Literal(lit) => {
                regex_str.push_str(&regex::escape(lit));
            }
            TemplateSegment::Field(field) => {
                let is_trailing = idx + 1 == total_segments;
                if is_trailing {
                    regex_str.push_str("(.*)");
                } else {
                    regex_str.push_str("(.+?)");
                }
                field_positions.push(*field);
            }
        }
    }
    regex_str.push('$');

    let re = Regex::new(&regex_str).context("failed to compile template regex")?;
    let stem = extract_filename_stem(filename_or_path);
    let caps = re
        .captures(&stem)
        .with_context(|| format!("filename '{stem}' does not match template '{template}'"))?;

    let mut patch = TagPatch::default();
    for (idx, field) in field_positions.into_iter().enumerate() {
        if let Some(m) = caps.get(idx + 1) {
            set_patch_field(&mut patch, field, m.as_str().trim().to_string());
        }
    }
    Ok(patch)
}

/// Parses tags from a filename or path using a regular expression and 1-based capture group mappings.
pub fn filename_to_tags_regex(
    filename_or_path: &str,
    regex_pattern: &str,
    group_mappings: &[(usize, TemplateField)],
) -> Result<TagPatch> {
    let re = Regex::new(regex_pattern).context("invalid filename regex")?;
    let stem = extract_filename_stem(filename_or_path);
    let caps = re
        .captures(&stem)
        .with_context(|| format!("filename '{stem}' did not match regex '{regex_pattern}'"))?;

    let mut patch = TagPatch::default();
    for &(group_idx, field) in group_mappings {
        if let Some(m) = caps.get(group_idx) {
            set_patch_field(&mut patch, field, m.as_str().trim().to_string());
        }
    }
    Ok(patch)
}

fn track_field_for_filename(track: &AudioTrack, field: TemplateField) -> String {
    let raw = match field {
        TemplateField::Ignore => return String::new(),
        TemplateField::Title => track.title.trim(),
        TemplateField::Artist => track.artist.trim(),
        TemplateField::Album => track.album.trim(),
        TemplateField::Disc => return pad_number_token(track.disc.trim()),
        TemplateField::Track => return pad_number_token(track.track.trim()),
        TemplateField::Year => track.year.trim(),
        TemplateField::Comment => track.comment.trim(),
        TemplateField::AlbumArtist => track.album_artist.trim(),
        TemplateField::Genre => track.genre.trim(),
        TemplateField::Composer => track.composer.trim(),
        TemplateField::Lyricist => track.lyricist.trim(),
    };
    sanitize_filename(raw)
}

fn pad_number_token(raw: &str) -> String {
    if raw.is_empty() {
        return String::new();
    }
    let head = raw.split('/').next().unwrap_or(raw).trim();
    if let Ok(n) = head.parse::<u32>() {
        if n < 10 {
            return format!("{n:02}");
        }
        return n.to_string();
    }
    sanitize_filename(head)
}

/// Returns any template fields whose values are empty on `track`.
pub fn missing_template_fields(track: &AudioTrack, template: &str) -> Vec<TemplateField> {
    let mut missing = Vec::new();
    for seg in parse_template(template) {
        if let TemplateSegment::Field(field) = seg {
            if field != TemplateField::Ignore
                && track_field_for_filename(track, field).is_empty()
                && !missing.contains(&field)
            {
                missing.push(field);
            }
        }
    }
    missing
}

/// Formats a track's metadata into a safe filename stem (without extension) using `template`.
pub fn tags_to_filename(track: &AudioTrack, template: &str) -> String {
    let mut out = String::new();
    for seg in parse_template(template) {
        match seg {
            TemplateSegment::Literal(lit) => out.push_str(&lit),
            TemplateSegment::Field(field) => out.push_str(&track_field_for_filename(track, field)),
        }
    }
    sanitize_filename(out.trim())
}

/// Replaces path/filesystem-unsafe characters so the result is valid across Linux/Windows/macOS.
pub fn sanitize_filename(name: &str) -> String {
    let mut cleaned = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_control() || matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
            cleaned.push('_');
        } else {
            cleaned.push(ch);
        }
    }
    let trimmed = cleaned.trim().trim_end_matches(['.', ' ']).to_string();
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        return String::new();
    }
    let upper = trimmed.to_ascii_uppercase();
    let is_reserved = matches!(
        upper.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    );
    if is_reserved {
        format!("_{trimmed}")
    } else {
        trimmed
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenameStatus {
    Ready,
    Unchanged,
    MissingField(String),
    Conflict(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidecarRename {
    pub from: PathBuf,
    pub to: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenamePlanItem {
    pub track_index: usize,
    pub from: PathBuf,
    pub to: PathBuf,
    pub new_filename: String,
    pub status: RenameStatus,
    pub sidecars: Vec<SidecarRename>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenameOutcome {
    pub track_index: usize,
    pub from: PathBuf,
    pub to: PathBuf,
    pub renamed_sidecars: Vec<SidecarRename>,
    pub error: Option<String>,
}

/// Plans batch file renames (and matching `.lrc`/`.jpg`/`.jpeg`/`.png` sidecars) from tags.
pub fn plan_batch_renames(
    tracks: &[AudioTrack],
    template: &str,
    rename_sidecars: bool,
) -> Vec<RenamePlanItem> {
    let mut planned_targets: HashSet<PathBuf> = HashSet::new();
    let mut items = Vec::with_capacity(tracks.len());

    for (idx, track) in tracks.iter().enumerate() {
        let missing = missing_template_fields(track, template);
        let parent = track.path.parent().unwrap_or_else(|| Path::new(""));
        let ext = track
            .path
            .extension()
            .and_then(|e| e.to_str())
            .filter(|e| !e.is_empty())
            .unwrap_or(track.ext.as_str());

        if !missing.is_empty() {
            let names: Vec<&str> = missing.iter().map(|f| f.display_name()).collect();
            items.push(RenamePlanItem {
                track_index: idx,
                from: track.path.clone(),
                to: track.path.clone(),
                new_filename: track.filename.clone(),
                status: RenameStatus::MissingField(names.join("、")),
                sidecars: Vec::new(),
            });
            continue;
        }

        let stem = tags_to_filename(track, template);
        if stem.is_empty() {
            items.push(RenamePlanItem {
                track_index: idx,
                from: track.path.clone(),
                to: track.path.clone(),
                new_filename: track.filename.clone(),
                status: RenameStatus::MissingField("目标文件名为空".to_string()),
                sidecars: Vec::new(),
            });
            continue;
        }

        let new_filename = if ext.is_empty() {
            stem.clone()
        } else {
            format!("{stem}.{ext}")
        };
        let target_path = parent.join(&new_filename);

        if target_path == track.path {
            items.push(RenamePlanItem {
                track_index: idx,
                from: track.path.clone(),
                to: target_path,
                new_filename,
                status: RenameStatus::Unchanged,
                sidecars: Vec::new(),
            });
            continue;
        }

        if !planned_targets.insert(target_path.clone()) || target_path.exists() {
            items.push(RenamePlanItem {
                track_index: idx,
                from: track.path.clone(),
                to: target_path,
                new_filename,
                status: RenameStatus::Conflict("目标文件已存在或名称冲突".to_string()),
                sidecars: Vec::new(),
            });
            continue;
        }

        let sidecars = if rename_sidecars {
            discover_sidecars(&track.path, parent, &stem)
        } else {
            Vec::new()
        };

        items.push(RenamePlanItem {
            track_index: idx,
            from: track.path.clone(),
            to: target_path,
            new_filename,
            status: RenameStatus::Ready,
            sidecars,
        });
    }

    items
}

fn discover_sidecars(audio_path: &Path, dir: &Path, new_stem: &str) -> Vec<SidecarRename> {
    let Some(old_stem) = audio_path.file_stem().and_then(|s| s.to_str()) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() || path == audio_path {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        if stem == old_stem
            && SIDECAR_EXTENSIONS
                .iter()
                .any(|&allowed| ext.eq_ignore_ascii_case(allowed))
        {
            let target = dir.join(format!("{new_stem}.{ext}"));
            if target != path {
                found.push(SidecarRename {
                    from: path,
                    to: target,
                });
            }
        }
    }
    found.sort_by(|a, b| a.from.cmp(&b.from));
    found
}

/// Executes all `RenameStatus::Ready` items in `plan` on disk.
pub fn execute_batch_renames(plan: &[RenamePlanItem]) -> Vec<RenameOutcome> {
    let mut outcomes = Vec::new();
    for item in plan {
        if item.status != RenameStatus::Ready {
            continue;
        }
        match std::fs::rename(&item.from, &item.to) {
            Ok(()) => {
                let mut renamed_sidecars = Vec::new();
                let mut sidecar_err = None;
                for sc in &item.sidecars {
                    if sc.from.exists() && !sc.to.exists() {
                        match std::fs::rename(&sc.from, &sc.to) {
                            Ok(()) => renamed_sidecars.push(sc.clone()),
                            Err(e) => {
                                sidecar_err = Some(format!(
                                    "sidecar rename failed ({} -> {}): {e}",
                                    sc.from.display(),
                                    sc.to.display()
                                ));
                            }
                        }
                    }
                }
                outcomes.push(RenameOutcome {
                    track_index: item.track_index,
                    from: item.from.clone(),
                    to: item.to.clone(),
                    renamed_sidecars,
                    error: sidecar_err,
                });
            }
            Err(e) => {
                outcomes.push(RenameOutcome {
                    track_index: item.track_index,
                    from: item.from.clone(),
                    to: item.to.clone(),
                    renamed_sidecars: Vec::new(),
                    error: Some(e.to_string()),
                });
            }
        }
    }
    outcomes
}

/// Maps UI `charset-encoding-index` (`0..=5`) to encoding label.
pub fn charset_label_from_index(idx: usize) -> &'static str {
    SUPPORTED_CHARSETS.get(idx).copied().unwrap_or("GBK")
}

/// Re-interprets garbled Latin-1/Windows-1252-decoded tag text as `source_encoding_label` -> UTF-8.
pub fn fix_garbled_text(garbled: &str, source_encoding_label: &str) -> Result<String> {
    if garbled.is_empty() {
        return Ok(String::new());
    }

    let raw_bytes: Vec<u8> = if garbled.chars().all(|c| (c as u32) <= 0xFF) {
        garbled.chars().map(|c| c as u8).collect()
    } else {
        let (cow, _, had_errors) = WINDOWS_1252.encode(garbled);
        if had_errors {
            garbled.as_bytes().to_vec()
        } else {
            cow.into_owned()
        }
    };

    let normalized_label = match source_encoding_label.trim().to_ascii_uppercase().as_str() {
        "ISO-8859-1" | "LATIN1" => "windows-1252",
        "GBK" => "gbk",
        "GB18030" => "gb18030",
        "BIG5" => "big5",
        "SHIFT_JIS" | "SHIFT-JIS" | "SJIS" => "shift_jis",
        "UTF-8" | "UTF8" => "utf-8",
        other => {
            return Encoding::for_label(other.as_bytes())
                .map(|enc| enc.decode(&raw_bytes).0.into_owned())
                .with_context(|| format!("unsupported charset: {source_encoding_label}"));
        }
    };

    let enc = Encoding::for_label(normalized_label.as_bytes())
        .with_context(|| format!("encoding_rs missing encoding {normalized_label}"))?;
    let (decoded, _, _) = enc.decode(&raw_bytes);
    Ok(decoded.into_owned())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CharsetFieldPreview {
    pub field_name: &'static str,
    pub original: String,
    pub converted: String,
}

/// Generates before/after charset previews for non-empty text fields on `track`.
pub fn preview_track_charset(
    track: &AudioTrack,
    source_encoding_label: &str,
) -> Result<Vec<CharsetFieldPreview>> {
    let fields = [
        ("标题", &track.title),
        ("艺术家", &track.artist),
        ("专辑", &track.album),
        ("专辑艺术家", &track.album_artist),
        ("流派", &track.genre),
        ("作曲", &track.composer),
        ("作词", &track.lyricist),
        ("注释", &track.comment),
    ];

    let mut rows = Vec::new();
    for (name, orig) in fields {
        if orig.is_empty() {
            continue;
        }
        let converted = fix_garbled_text(orig, source_encoding_label)?;
        rows.push(CharsetFieldPreview {
            field_name: name,
            original: orig.clone(),
            converted,
        });
    }
    Ok(rows)
}

/// Converts garbled text fields on `track` into a `TagPatch` with UTF-8 strings.
pub fn convert_track_charset_patch(
    track: &AudioTrack,
    source_encoding_label: &str,
) -> Result<TagPatch> {
    let mut patch = TagPatch::default();
    let convert_opt = |orig: &str| -> Result<FieldUpdate<String>> {
        if orig.is_empty() {
            return Ok(FieldUpdate::Keep);
        }
        let fixed = fix_garbled_text(orig, source_encoding_label)?;
        if fixed == orig {
            Ok(FieldUpdate::Keep)
        } else {
            Ok(FieldUpdate::Set(fixed))
        }
    };

    patch.title = convert_opt(&track.title)?;
    patch.artist = convert_opt(&track.artist)?;
    patch.album = convert_opt(&track.album)?;
    patch.album_artist = convert_opt(&track.album_artist)?;
    patch.genre = convert_opt(&track.genre)?;
    patch.composer = convert_opt(&track.composer)?;
    patch.lyricist = convert_opt(&track.lyricist)?;
    if !track.has_163_key {
        patch.comment = convert_opt(&track.comment)?;
    }
    Ok(patch)
}

/// Converts Simplified/Traditional Chinese text.
pub fn convert_text_zh(text: &str, mode: ZhConvMode) -> String {
    convert_zh(text, mode)
}

/// Creates a `TagPatch` converting Chinese script across text fields (and optionally lyrics).
pub fn convert_track_zh_patch(
    track: &AudioTrack,
    mode: ZhConvMode,
    include_lyrics: bool,
) -> TagPatch {
    if mode == ZhConvMode::None {
        return TagPatch::default();
    }
    let conv = |orig: &str| -> FieldUpdate<String> {
        if orig.is_empty() {
            return FieldUpdate::Keep;
        }
        let out = convert_zh(orig, mode);
        if out == orig {
            FieldUpdate::Keep
        } else {
            FieldUpdate::Set(out)
        }
    };

    TagPatch {
        title: conv(&track.title),
        artist: conv(&track.artist),
        album: conv(&track.album),
        album_artist: conv(&track.album_artist),
        genre: conv(&track.genre),
        composer: conv(&track.composer),
        lyricist: conv(&track.lyricist),
        comment: if track.has_163_key {
            FieldUpdate::Keep
        } else {
            conv(&track.comment)
        },
        lyrics: if include_lyrics {
            conv(&track.lyrics)
        } else {
            FieldUpdate::Keep
        },
        ..TagPatch::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn sample_track(path: PathBuf) -> AudioTrack {
        AudioTrack {
            path,
            filename: "test.flac".to_string(),
            ext: "flac".to_string(),
            title: "だから僕は音楽を辞めた".to_string(),
            artist: "ヨルシカ".to_string(),
            album: "だから僕は音楽を辞めた".to_string(),
            album_artist: "ヨルシカ".to_string(),
            year: "2019".to_string(),
            track: "3".to_string(),
            disc: "1".to_string(),
            comment: "note".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn template_filename_to_tags_works_for_presets_and_positional() {
        let p1 = filename_to_tags(
            "/music/初音ミク;n-buna - ウミユリ海底譚.flac",
            "%artist% - %title%",
        )
        .unwrap();
        assert_eq!(
            p1.artist,
            FieldUpdate::Set("初音ミク;n-buna".to_string())
        );
        assert_eq!(p1.title, FieldUpdate::Set("ウミユリ海底譚".to_string()));

        let p2 = filename_to_tags("03. 夜に駆ける.mp3", "%track%. %title%").unwrap();
        assert_eq!(p2.track, FieldUpdate::Set("03".to_string()));
        assert_eq!(p2.title, FieldUpdate::Set("夜に駆ける".to_string()));

        let p3 = filename_to_tags(
            "ignore - My Title - My Artist - My Album.flac",
            "@0 - @1 - @2 - @3",
        )
        .unwrap();
        assert_eq!(p3.title, FieldUpdate::Set("My Title".to_string()));
        assert_eq!(p3.artist, FieldUpdate::Set("My Artist".to_string()));
        assert_eq!(p3.album, FieldUpdate::Set("My Album".to_string()));
    }

    #[test]
    fn regex_filename_to_tags_extracts_capture_groups() {
        let patch = filename_to_tags_regex(
            "03. xxartist - xxtitle.flac",
            r"^(\d*)\. (.*) - (.*)$",
            &[
                (1, TemplateField::Track),
                (2, TemplateField::Artist),
                (3, TemplateField::Title),
            ],
        )
        .unwrap();

        assert_eq!(patch.track, FieldUpdate::Set("03".to_string()));
        assert_eq!(patch.artist, FieldUpdate::Set("xxartist".to_string()));
        assert_eq!(patch.title, FieldUpdate::Set("xxtitle".to_string()));
    }

    #[test]
    fn tags_to_filename_and_sanitizer() {
        let mut track = sample_track(PathBuf::from("/tmp/raw.flac"));
        track.title = "Fate/stay: night?".to_string();
        let name = tags_to_filename(&track, "%track%. %artist% - %title%");
        assert_eq!(name, "03. ヨルシカ - Fate_stay_ night_");

        assert_eq!(sanitize_filename("CON"), "_CON");
        assert_eq!(sanitize_filename("a<b>c:d\\e/f|g?h*i."), "a_b_c_d_e_f_g_h_i");
    }

    #[test]
    fn batch_rename_planner_and_executor_with_sidecars() {
        let dir = tempdir().unwrap();
        let audio = dir.path().join("old_song.flac");
        let lrc = dir.path().join("old_song.lrc");
        let jpg = dir.path().join("old_song.jpg");
        let png = dir.path().join("old_song.PNG");
        std::fs::write(&audio, b"audio").unwrap();
        std::fs::write(&lrc, b"lrc").unwrap();
        std::fs::write(&jpg, b"jpg").unwrap();
        std::fs::write(&png, b"png").unwrap();

        let mut track1 = sample_track(audio.clone());
        track1.filename = "old_song.flac".to_string();

        let mut track2 = sample_track(dir.path().join("missing.flac"));
        track2.track.clear();

        let plan = plan_batch_renames(&[track1, track2], "%track%. %artist% - %title%", true);
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].status, RenameStatus::Ready);
        assert_eq!(
            plan[0].new_filename,
            "03. ヨルシカ - だから僕は音楽を辞めた.flac"
        );
        assert_eq!(plan[0].sidecars.len(), 3);
        assert!(matches!(plan[1].status, RenameStatus::MissingField(_)));

        let outcomes = execute_batch_renames(&plan);
        assert_eq!(outcomes.len(), 1);
        assert!(outcomes[0].error.is_none());
        assert!(!audio.exists());
        assert!(plan[0].to.exists());
        assert!(dir
            .path()
            .join("03. ヨルシカ - だから僕は音楽を辞めた.lrc")
            .exists());
        assert!(dir
            .path()
            .join("03. ヨルシカ - だから僕は音楽を辞めた.jpg")
            .exists());
        assert!(dir
            .path()
            .join("03. ヨルシカ - だから僕は音楽を辞めた.PNG")
            .exists());
    }

    #[test]
    fn charset_fix_recovers_gbk_mojibake_and_zhconv_converts() {
        let fixed = fix_garbled_text("Ó°Ö®ÈÐÁã - ³àºÚ´«³Ð", "GBK").unwrap();
        assert_eq!(fixed, "影之刃零 - 赤黑传承");

        let artist_fixed = fix_garbled_text("±¡²ÊÉú", "GBK").unwrap();
        assert_eq!(artist_fixed, "薄彩生");

        assert_eq!(
            convert_text_zh("影之刃零原聲曲輯", ZhConvMode::ToSimplified),
            "影之刃零原声曲辑"
        );
        assert_eq!(
            convert_text_zh("网易云音乐标签", ZhConvMode::ToTraditional),
            "網易雲音樂標籤"
        );
    }
}
