use std::cmp::Ordering;
use std::collections::HashMap;

use regex::Regex;
use serde::Deserialize;
use zhconv::{Variant, zhconv};

use crate::core::models::{LyricProcessOptions, ProcessedLyrics, ZhConvMode};

const CREDIT_KEYWORDS: &[&str] = &[
    "作词",
    "作曲",
    "编曲",
    "词",
    "曲",
    "词曲",
    "制作人",
    "混音",
    "母带",
    "录音",
    "和声",
    "吉他",
    "贝斯",
    "鼓",
    "钢琴",
    "弦乐",
    "演唱",
    "监制",
    "统筹",
    "出品",
    "发行",
    "op",
    "sp",
    "lyricist",
    "composer",
    "arranger",
    "producer",
];

const LRC_HEAD_PREFIXES: &[&str] = &[
    "[ti:",
    "[ar:",
    "[al:",
    "[by:",
    "[offset:",
    "[re:",
    "[ve:",
    "[length:",
];

#[derive(Debug, Deserialize)]
struct NetEaseJsonLyricLine {
    t: Option<u64>,
    c: Option<Vec<NetEaseJsonLyricChunk>>,
}

#[derive(Debug, Deserialize)]
struct NetEaseJsonLyricChunk {
    tx: Option<String>,
}

#[derive(Debug, Clone)]
struct ParsedLine {
    ts_ms: Option<u64>,
    raw_ts: Option<String>,
    text: String,
}

/// Processes NetEase original and translated lyrics according to `LyricProcessOptions`.
pub fn process_netease_lyrics(
    raw_lrc: Option<&str>,
    raw_tlyric: Option<&str>,
    nolyric: bool,
    track_title: &str,
    opts: &LyricProcessOptions,
) -> ProcessedLyrics {
    let orig_raw = raw_lrc.unwrap_or("");
    let trans_raw = raw_tlyric.unwrap_or("");

    let normalized_orig = normalize_netease_json_lines(orig_raw);
    let normalized_trans = normalize_netease_json_lines(trans_raw);

    if nolyric
        || contains_instrumental_phrase(orig_raw)
        || contains_instrumental_phrase(&normalized_orig)
        || (opts.skip_instrumental && title_looks_instrumental(track_title))
    {
        return ProcessedLyrics::InstrumentalSkipped;
    }

    let orig_cleaned = apply_line_cleanup(&normalized_orig, opts);
    let trans_cleaned = if opts.dl_trans {
        apply_line_cleanup(&normalized_trans, opts)
    } else {
        String::new()
    };

    let orig_lines = parse_lrc_lines(&orig_cleaned, opts.reformat_timetag);
    let trans_lines = parse_lrc_lines(&trans_cleaned, opts.reformat_timetag);

    if orig_lines.is_empty() && trans_lines.is_empty() {
        return ProcessedLyrics::Empty;
    }

    let mut combined = if opts.dl_trans && opts.dont_dl_orig && !trans_lines.is_empty() {
        format_single_lines(&trans_lines)
    } else if !opts.dl_trans || trans_lines.is_empty() {
        if orig_lines.is_empty() {
            return ProcessedLyrics::Empty;
        }
        format_single_lines(&orig_lines)
    } else {
        align_bilingual(&orig_lines, &trans_lines, opts)
    };

    if opts.remove_timetag {
        combined = remove_timetags(&combined);
        if opts.delete_blank_lines {
            combined = combined
                .lines()
                .filter(|line| !line.trim().is_empty())
                .collect::<Vec<_>>()
                .join("\r\n");
        }
    }

    combined = convert_zh(&combined, opts.zh_conv);

    if combined.trim().is_empty() {
        ProcessedLyrics::Empty
    } else {
        ProcessedLyrics::Lyrics(combined)
    }
}

fn contains_instrumental_phrase(text: &str) -> bool {
    text.contains("纯音乐，请欣赏")
        || text.contains("纯音乐,请欣赏")
        || text.contains("没有填词的纯音乐")
}

fn title_looks_instrumental(title: &str) -> bool {
    let lower = title.to_ascii_lowercase();
    lower.contains("instrumental")
        || lower.contains("off vocal")
        || title.contains("伴奏")
        || title.contains("纯音乐")
}

/// Converts NetEase JSON lyric lines (`{"t":123,"c":[{"tx":"..."}]}`) into standard `[MM:SS.xxx]text`.
pub fn normalize_netease_json_lines(raw: &str) -> String {
    raw.lines()
        .map(|line| {
            let trimmed = line.trim();
            if trimmed.starts_with('{') && trimmed.ends_with('}') {
                if let Ok(parsed) = serde_json::from_str::<NetEaseJsonLyricLine>(trimmed) {
                    if let (Some(ms), Some(chunks)) = (parsed.t, parsed.c) {
                        let text: String = chunks.into_iter().filter_map(|c| c.tx).collect();
                        return format!("{}{}", format_ms_3(ms), text.trim_end());
                    }
                }
            }
            line.trim_end_matches('\r').to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn apply_line_cleanup(raw: &str, opts: &LyricProcessOptions) -> String {
    let mut text = raw.replace("\r\n", "\n").replace('\r', "\n");
    if opts.delete_head_tag {
        text = delete_head_tags(&text);
    }
    if opts.reformat_timetag {
        text = reformat_timetags(&text);
    }
    if opts.delete_blank_lines {
        text = delete_blank_lines(&text);
    }
    text
}

fn format_single_lines(lines: &[ParsedLine]) -> String {
    lines
        .iter()
        .map(|l| format_parsed_line(l))
        .collect::<Vec<_>>()
        .join("\r\n")
}

fn format_parsed_line(line: &ParsedLine) -> String {
    match &line.raw_ts {
        Some(ts) => format!("{ts}{}", line.text),
        None => line.text.clone(),
    }
}

fn align_bilingual(
    orig: &[ParsedLine],
    trans: &[ParsedLine],
    opts: &LyricProcessOptions,
) -> String {
    match opts.trans_format {
        2 => align_interleaved(orig, trans),
        1 | 3 | 0 => align_matched(orig, trans, opts.trans_format, &opts.trans_connector),
        _ => align_matched(orig, trans, 0, &opts.trans_connector),
    }
}

fn align_interleaved(orig: &[ParsedLine], trans: &[ParsedLine]) -> String {
    let mut timed: Vec<(u64, bool, usize, &ParsedLine)> = Vec::new();
    let mut untimed: Vec<&ParsedLine> = Vec::new();

    for (idx, line) in orig.iter().enumerate() {
        if let Some(ms) = line.ts_ms {
            timed.push((ms, false, idx, line));
        } else {
            untimed.push(line);
        }
    }
    for (idx, line) in trans.iter().enumerate() {
        if let Some(ms) = line.ts_ms {
            timed.push((ms, true, idx, line));
        } else {
            untimed.push(line);
        }
    }

    timed.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2.cmp(&b.2))
    });

    let mut out: Vec<String> = timed.into_iter().map(|(_, _, _, l)| format_parsed_line(l)).collect();
    for line in untimed {
        out.push(format_parsed_line(line));
    }
    out.join("\r\n")
}

fn align_matched(
    orig: &[ParsedLine],
    trans: &[ParsedLine],
    trans_format: u8,
    connector_raw: &str,
) -> String {
    let mut exact_map: HashMap<u64, Vec<usize>> = HashMap::new();
    let mut centi_map: HashMap<u64, Vec<usize>> = HashMap::new();
    let mut used = vec![false; trans.len()];

    for (idx, t) in trans.iter().enumerate() {
        if let Some(ms) = t.ts_ms {
            exact_map.entry(ms).or_default().push(idx);
            centi_map.entry(ms / 10).or_default().push(idx);
        }
    }

    let connector = unescape_connector(connector_raw);
    let mut out = Vec::with_capacity(orig.len() * 2);

    for o in orig {
        let matched_idx = o.ts_ms.and_then(|ms| {
            take_unused(exact_map.get(&ms), &mut used)
                .or_else(|| take_unused(centi_map.get(&(ms / 10)), &mut used))
        });

        let ts_prefix = o.raw_ts.as_deref().unwrap_or("");
        match matched_idx {
            Some(ti) => {
                let t_text = &trans[ti].text;
                if t_text.is_empty() {
                    out.push(format!("{ts_prefix}{}", o.text));
                } else {
                    match trans_format {
                        1 => {
                            out.push(format!("{ts_prefix}{}{connector}{t_text}", o.text));
                        }
                        3 => {
                            out.push(format!("{ts_prefix}{t_text}"));
                            out.push(format!("{ts_prefix}{}", o.text));
                        }
                        _ => {
                            out.push(format!("{ts_prefix}{}", o.text));
                            out.push(format!("{ts_prefix}{t_text}"));
                        }
                    }
                }
            }
            None => {
                out.push(format!("{ts_prefix}{}", o.text));
            }
        }
    }

    out.join("\r\n")
}

fn take_unused(candidates: Option<&Vec<usize>>, used: &mut [bool]) -> Option<usize> {
    let list = candidates?;
    for &idx in list {
        if !used[idx] {
            used[idx] = true;
            return Some(idx);
        }
    }
    None
}

fn unescape_connector(raw: &str) -> String {
    raw.replace("\\r\\n", "\r\n")
        .replace("\\n", "\n")
        .replace("\\r", "\r")
        .replace("\\t", "\t")
}

fn parse_lrc_lines(input: &str, reformat: bool) -> Vec<ParsedLine> {
    let mut lines = Vec::new();
    for raw_line in input.lines() {
        let line = raw_line.trim_end_matches('\r');
        if line.trim().is_empty() {
            continue;
        }
        let (tags, text) = extract_leading_timetags(line);
        if tags.is_empty() {
            lines.push(ParsedLine {
                ts_ms: None,
                raw_ts: None,
                text: text.to_string(),
            });
        } else {
            for (ms, raw_tag) in tags {
                let formatted_ts = if reformat {
                    format_ms_2(ms)
                } else {
                    raw_tag
                };
                lines.push(ParsedLine {
                    ts_ms: Some(ms),
                    raw_ts: Some(formatted_ts),
                    text: text.to_string(),
                });
            }
        }
    }
    lines.sort_by(|a, b| match (a.ts_ms, b.ts_ms) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    });
    lines
}

fn extract_leading_timetags(line: &str) -> (Vec<(u64, String)>, &str) {
    let mut rest = line;
    let mut tags = Vec::new();
    while rest.starts_with('[') {
        let Some(end) = rest.find(']') else {
            break;
        };
        let inside = &rest[1..end];
        if let Some(ms) = parse_timetag_ms(inside) {
            tags.push((ms, rest[..=end].to_string()));
            rest = &rest[end + 1..];
        } else {
            break;
        }
    }
    (tags, rest)
}

fn parse_timetag_ms(inside: &str) -> Option<u64> {
    let (mins_str, sec_part) = inside.split_once(':')?;
    if mins_str.is_empty() || !mins_str.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mins: u64 = mins_str.parse().ok()?;

    let (secs_str, frac_ms) = if let Some((s, frac)) = sec_part.split_once('.') {
        if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        if frac.is_empty() || !frac.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let ms = match frac.len() {
            1 => frac.parse::<u64>().ok()? * 100,
            2 => frac.parse::<u64>().ok()? * 10,
            3 => frac.parse::<u64>().ok()?,
            _ => frac[..3].parse::<u64>().ok()?,
        };
        (s, ms)
    } else {
        if sec_part.is_empty() || !sec_part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        (sec_part, 0)
    };

    let secs: u64 = secs_str.parse().ok()?;
    Some(mins * 60_000 + secs * 1_000 + frac_ms)
}

fn format_ms_2(ms: u64) -> String {
    let total_cs = ms / 10;
    let mins = total_cs / 6000;
    let rem = total_cs % 6000;
    let secs = rem / 100;
    let cs = rem % 100;
    format!("[{mins:02}:{secs:02}.{cs:02}]")
}

fn format_ms_3(ms: u64) -> String {
    let mins = ms / 60_000;
    let rem = ms % 60_000;
    let secs = rem / 1000;
    let millis = rem % 1000;
    format!("[{mins:02}:{secs:02}.{millis:03}]")
}

fn is_lrc_head_tag(trimmed: &str) -> bool {
    let lower = trimmed.to_ascii_lowercase();
    LRC_HEAD_PREFIXES.iter().any(|p| lower.starts_with(p)) && trimmed.ends_with(']')
}

fn is_credit_text(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }
    let colon_pos = trimmed
        .find(':')
        .into_iter()
        .chain(trimmed.find('：'))
        .min();
    let Some(pos) = colon_pos else {
        return false;
    };
    let label = trimmed[..pos].trim();
    if label.is_empty() || label.chars().count() > 24 {
        return false;
    }

    let parts: Vec<&str> = label
        .split(['/', '、', '&', '・', ' '])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    !parts.is_empty()
        && parts.iter().all(|part| {
            let lower = part.to_ascii_lowercase();
            CREDIT_KEYWORDS.iter().any(|&kw| lower == kw)
        })
}

/// Strips LRC header tags (`[ti:]`, `[ar:]`, `[al:]`, `[by:]`, `[offset:]`, ...) and leading credit lines.
pub fn delete_head_tags(lyrics: &str) -> String {
    let mut in_header = true;
    let mut kept = Vec::new();

    for raw_line in lyrics.lines() {
        let line = raw_line.trim_end_matches('\r');
        let trimmed = line.trim();
        if is_lrc_head_tag(trimmed) {
            continue;
        }
        let body = remove_timetags(trimmed);
        let body_trimmed = body.trim();
        if in_header {
            if body_trimmed.is_empty() {
                if !kept.is_empty() {
                    kept.push(line.to_string());
                }
                continue;
            }
            if is_credit_text(body_trimmed) {
                continue;
            }
            in_header = false;
        }
        kept.push(line.to_string());
    }

    kept.join("\n")
}

/// Removes lines whose lyric text (after stripping timetags) is empty or whitespace.
pub fn delete_blank_lines(lyrics: &str) -> String {
    lyrics
        .lines()
        .map(|l| l.trim_end_matches('\r'))
        .filter(|line| !remove_timetags(line).trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Normalizes timestamps (`[MM:SS.xxx]`, `[M:SS.xx]`, `[MM:SS]`) to `[MM:SS.xx]`.
pub fn reformat_timetags(lyrics: &str) -> String {
    let re = Regex::new(r"\[(\d{1,3}):(\d{1,2})(?:\.(\d{1,3}))?\]").expect("valid timetag regex");
    lyrics
        .lines()
        .map(|l| l.trim_end_matches('\r'))
        .map(|line| {
            re.replace_all(line, |caps: &regex::Captures<'_>| {
                let mins: u64 = caps[1].parse().unwrap_or(0);
                let secs: u64 = caps[2].parse().unwrap_or(0);
                let frac_ms: u64 = match caps.get(3).map(|m| m.as_str()) {
                    Some(f) if f.len() == 1 => f.parse::<u64>().unwrap_or(0) * 100,
                    Some(f) if f.len() == 2 => f.parse::<u64>().unwrap_or(0) * 10,
                    Some(f) if f.len() >= 3 => f[..3].parse::<u64>().unwrap_or(0),
                    _ => 0,
                };
                format_ms_2(mins * 60_000 + secs * 1_000 + frac_ms)
            })
            .into_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Removes all `[MM:SS...]` timetags from `lyrics`, preserving non-timetag `[...]` markers.
pub fn remove_timetags(lyrics: &str) -> String {
    let re = Regex::new(r"\[\d{1,3}:\d{1,2}(?:\.\d{1,3})?\]").expect("valid timetag regex");
    lyrics
        .lines()
        .map(|l| l.trim_end_matches('\r'))
        .map(|line| re.replace_all(line, "").into_owned())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Shifts all `[MM:SS...]` timetags by `offset_ms` milliseconds (clamped to `>= 0`).
pub fn shift_timetags(lyrics: &str, offset_ms: i64) -> String {
    let re = Regex::new(r"\[(\d{1,3}):(\d{1,2})(?:\.(\d{1,3}))?\]").expect("valid timetag regex");
    lyrics
        .lines()
        .map(|l| l.trim_end_matches('\r'))
        .map(|line| {
            re.replace_all(line, |caps: &regex::Captures<'_>| {
                let mins: i64 = caps[1].parse().unwrap_or(0);
                let secs: i64 = caps[2].parse().unwrap_or(0);
                let frac_ms: i64 = match caps.get(3).map(|m| m.as_str()) {
                    Some(f) if f.len() == 1 => f.parse::<i64>().unwrap_or(0) * 100,
                    Some(f) if f.len() == 2 => f.parse::<i64>().unwrap_or(0) * 10,
                    Some(f) if f.len() >= 3 => f[..3].parse::<i64>().unwrap_or(0),
                    _ => 0,
                };
                let total_ms = (mins * 60_000 + secs * 1_000 + frac_ms + offset_ms).max(0) as u64;
                format_ms_2(total_ms)
            })
            .into_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Converts Chinese text between Simplified (`zh-Hans`) and Traditional (`zh-Hant`).
pub fn convert_zh(text: &str, mode: ZhConvMode) -> String {
    match mode {
        ZhConvMode::None => text.to_string(),
        ZhConvMode::ToSimplified => zhconv(text, Variant::ZhHans),
        ZhConvMode::ToTraditional => zhconv(text, Variant::ZhHant),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instrumental_detection_works() {
        let opts = LyricProcessOptions::default();

        assert_eq!(
            process_netease_lyrics(
                Some("[00:00.00]普通歌词"),
                None,
                true,
                "Some Song",
                &opts,
            ),
            ProcessedLyrics::InstrumentalSkipped
        );

        assert_eq!(
            process_netease_lyrics(
                Some("[00:00.00]纯音乐，请欣赏"),
                None,
                false,
                "Merry Christmas Mr. Lawrence",
                &opts,
            ),
            ProcessedLyrics::InstrumentalSkipped
        );

        assert_eq!(
            process_netease_lyrics(
                Some("[00:00.00]纯音乐,请欣赏"),
                None,
                false,
                "Track",
                &opts,
            ),
            ProcessedLyrics::InstrumentalSkipped
        );

        assert_eq!(
            process_netease_lyrics(
                Some("[00:00.00]没有填词的纯音乐"),
                None,
                false,
                "Track",
                &opts,
            ),
            ProcessedLyrics::InstrumentalSkipped
        );

        assert_eq!(
            process_netease_lyrics(
                Some("[00:01.00]la la la"),
                None,
                false,
                "Yorushika (Instrumental)",
                &opts,
            ),
            ProcessedLyrics::InstrumentalSkipped
        );

        assert_eq!(
            process_netease_lyrics(
                Some("[00:01.00]la la la"),
                None,
                false,
                "Track (Off Vocal)",
                &opts,
            ),
            ProcessedLyrics::InstrumentalSkipped
        );
    }

    #[test]
    fn json_credits_and_head_tags_stripped_and_bilingual_format_0_aligned() {
        let raw_lrc = concat!(
            "[ti:だから僕は音楽を辞めた]\n",
            "[ar:ヨルシカ]\n",
            "{\"t\":0,\"c\":[{\"tx\":\"作词 : \"},{\"tx\":\"n-buna\"}]}\n",
            "{\"t\":500,\"c\":[{\"tx\":\"作曲 : \"},{\"tx\":\"n-buna\"}]}\n",
            "[00:00.800]编曲 : n-buna\n",
            "[00:01.085]\n",
            "[00:01.085]考えたってわからないし\n",
            "[00:03.060]青空の下、君を待った\n"
        );
        let raw_tlyric = concat!(
            "[by:translator]\n",
            "[00:01.08]想过之后依然搞不懂\n",
            "[00:03.060]在蔚蓝的天空下等待着你\n"
        );

        let opts = LyricProcessOptions::default();
        let res = process_netease_lyrics(
            Some(raw_lrc),
            Some(raw_tlyric),
            false,
            "だから僕は音楽を辞めた",
            &opts,
        );

        let expected = concat!(
            "[00:01.08]考えたってわからないし\r\n",
            "[00:01.08]想过之后依然搞不懂\r\n",
            "[00:03.06]青空の下、君を待った\r\n",
            "[00:03.06]在蔚蓝的天空下等待着你"
        );
        assert_eq!(res, ProcessedLyrics::Lyrics(expected.to_string()));
    }

    #[test]
    fn bilingual_formats_1_2_3_and_dont_dl_orig() {
        let orig = "[00:05.12]原词1\n[00:07.06]原词2";
        let trans = "[00:05.12]翻译1\n[00:07.05]翻译2";

        // Format 1 with custom connector
        let mut opts = LyricProcessOptions {
            trans_format: 1,
            trans_connector: " / ".to_string(),
            ..Default::default()
        };
        assert_eq!(
            process_netease_lyrics(Some(orig), Some(trans), false, "Song", &opts),
            ProcessedLyrics::Lyrics("[00:05.12]原词1 / 翻译1\r\n[00:07.06]原词2".to_string())
        );

        // Format 2 interleaved at original timestamps
        opts.trans_format = 2;
        assert_eq!(
            process_netease_lyrics(Some(orig), Some(trans), false, "Song", &opts),
            ProcessedLyrics::Lyrics(
                "[00:05.12]原词1\r\n[00:05.12]翻译1\r\n[00:07.05]翻译2\r\n[00:07.06]原词2"
                    .to_string()
            )
        );

        // Format 3 translation above original
        opts.trans_format = 3;
        assert_eq!(
            process_netease_lyrics(
                Some("[00:05.12]原词1"),
                Some("[00:05.12]翻译1"),
                false,
                "Song",
                &opts
            ),
            ProcessedLyrics::Lyrics("[00:05.12]翻译1\r\n[00:05.12]原词1".to_string())
        );

        // dont_dl_orig
        opts.dont_dl_orig = true;
        assert_eq!(
            process_netease_lyrics(Some(orig), Some(trans), false, "Song", &opts),
            ProcessedLyrics::Lyrics("[00:05.12]翻译1\r\n[00:07.05]翻译2".to_string())
        );
    }

    #[test]
    fn remove_timetag_and_zhconv() {
        let orig = "[00:05.12]繁體歌詞測試";
        let opts = LyricProcessOptions {
            remove_timetag: true,
            zh_conv: ZhConvMode::ToSimplified,
            ..Default::default()
        };
        assert_eq!(
            process_netease_lyrics(Some(orig), None, false, "Song", &opts),
            ProcessedLyrics::Lyrics("繁体歌词测试".to_string())
        );

        assert_eq!(
            convert_zh("简体中文测试", ZhConvMode::ToTraditional),
            "簡體中文測試"
        );
    }

    #[test]
    fn standalone_editor_utilities_work() {
        let raw = "[ti:Title]\n[00:00.00]Lyricist: Foo\n[00:01.085]Hello\n[00:02.000]\n[00:03.500]World";
        let no_head = delete_head_tags(raw);
        assert_eq!(no_head, "[00:01.085]Hello\n[00:02.000]\n[00:03.500]World");

        let no_blank = delete_blank_lines(&no_head);
        assert_eq!(no_blank, "[00:01.085]Hello\n[00:03.500]World");

        let reformatted = reformat_timetags(&no_blank);
        assert_eq!(reformatted, "[00:01.08]Hello\n[00:03.50]World");

        let shifted = shift_timetags(&reformatted, -1500);
        assert_eq!(shifted, "[00:00.00]Hello\n[00:02.00]World");

        let stripped = remove_timetags(&reformatted);
        assert_eq!(stripped, "Hello\nWorld");
    }

    #[test]
    fn multi_timetag_and_newline_connector() {
        let orig = "[00:10.00][01:20.00]Chorus";
        let trans = "[00:10.00][01:20.00]副歌";
        let opts = LyricProcessOptions {
            trans_format: 1,
            trans_connector: "\\n".to_string(),
            ..Default::default()
        };
        assert_eq!(
            process_netease_lyrics(Some(orig), Some(trans), false, "Song", &opts),
            ProcessedLyrics::Lyrics("[00:10.00]Chorus\n副歌\r\n[01:20.00]Chorus\n副歌".to_string())
        );
    }
}
