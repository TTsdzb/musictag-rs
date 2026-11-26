use slint::{Image, Rgba8Pixel, SharedPixelBuffer};

use crate::{
    AutoMatchRow, CharsetPreviewRow, ColumnConfigItem, CoverCandidate, DirEntry,
    FilenamePreviewItem, HistoryEntry, RegexGroupItem, SearchCandidate, TrackItem,
};

#[derive(Clone, Copy)]
pub struct CoverPalette {
    pub bg_top: [u8; 3],
    pub bg_bottom: [u8; 3],
    pub accent: [u8; 3],
    pub highlight: [u8; 3],
    pub pattern: u8,
}

/// Generates a crisp procedural geometric album cover thumbnail.
pub fn make_cover(size: u32, palette: CoverPalette) -> Image {
    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(size, size);
    let pixels = buf.make_mut_slice();
    let s = size as f32;

    for y in 0..size {
        let ty = y as f32 / s;
        for x in 0..size {
            let tx = x as f32 / s;
            let mut r = lerp(palette.bg_top[0], palette.bg_bottom[0], ty * 0.7 + tx * 0.3);
            let mut g = lerp(palette.bg_top[1], palette.bg_bottom[1], ty * 0.7 + tx * 0.3);
            let mut b = lerp(palette.bg_top[2], palette.bg_bottom[2], ty * 0.7 + tx * 0.3);

            let dx = tx - 0.5;
            let dy = ty - 0.5;
            let dist = (dx * dx + dy * dy).sqrt();

            match palette.pattern % 4 {
                0 => {
                    // Concentric vinyl/wave rings + glowing sun/moon disc
                    if dist < 0.28 {
                        let k = 1.0 - (dist / 0.28) * 0.25;
                        r = lerp_f(r, palette.accent[0], k);
                        g = lerp_f(g, palette.accent[1], k);
                        b = lerp_f(b, palette.accent[2], k);
                    } else if (dist - 0.34).abs() < 0.012 || (dist - 0.41).abs() < 0.008 {
                        r = lerp_f(r, palette.highlight[0], 0.85);
                        g = lerp_f(g, palette.highlight[1], 0.85);
                        b = lerp_f(b, palette.highlight[2], 0.85);
                    }
                    // Horizon wave band
                    let wave = 0.68 + 0.045 * (tx * 11.0).sin();
                    if ty > wave && ty < wave + 0.035 {
                        r = lerp_f(r, palette.highlight[0], 0.65);
                        g = lerp_f(g, palette.highlight[1], 0.65);
                        b = lerp_f(b, palette.highlight[2], 0.65);
                    }
                }
                1 => {
                    // Diagonal prism bands + geometric diamond
                    let diag = (tx + ty) * 0.5;
                    if (diag - 0.48).abs() < 0.11 {
                        r = lerp_f(r, palette.accent[0], 0.72);
                        g = lerp_f(g, palette.accent[1], 0.72);
                        b = lerp_f(b, palette.accent[2], 0.72);
                    }
                    let manhattan = dx.abs() + dy.abs();
                    if manhattan < 0.24 {
                        r = lerp_f(r, palette.highlight[0], 0.82);
                        g = lerp_f(g, palette.highlight[1], 0.82);
                        b = lerp_f(b, palette.highlight[2], 0.82);
                    }
                }
                2 => {
                    // Minimalist offset circle + vertical editorial bars
                    let off_dist = ((tx - 0.60).powi(2) + (ty - 0.42).powi(2)).sqrt();
                    if off_dist < 0.25 {
                        r = lerp_f(r, palette.accent[0], 0.88);
                        g = lerp_f(g, palette.accent[1], 0.88);
                        b = lerp_f(b, palette.accent[2], 0.88);
                    }
                    if tx > 0.14 && tx < 0.22 && ty > 0.16 && ty < 0.84 {
                        r = lerp_f(r, palette.highlight[0], 0.78);
                        g = lerp_f(g, palette.highlight[1], 0.78);
                        b = lerp_f(b, palette.highlight[2], 0.78);
                    }
                }
                _ => {
                    // Grid waveform + center emblem
                    let grid_x = ((tx * 12.0).fract() - 0.5).abs();
                    let grid_y = ((ty * 12.0).fract() - 0.5).abs();
                    if grid_x > 0.46 || grid_y > 0.46 {
                        r = lerp_f(r, palette.accent[0], 0.22);
                        g = lerp_f(g, palette.accent[1], 0.22);
                        b = lerp_f(b, palette.accent[2], 0.22);
                    }
                    if dx.abs() < 0.22 && dy.abs() < 0.22 {
                        let ring = dx.abs().max(dy.abs());
                        if ring > 0.15 {
                            r = lerp_f(r, palette.highlight[0], 0.85);
                            g = lerp_f(g, palette.highlight[1], 0.85);
                            b = lerp_f(b, palette.highlight[2], 0.85);
                        } else {
                            r = lerp_f(r, palette.accent[0], 0.75);
                            g = lerp_f(g, palette.accent[1], 0.75);
                            b = lerp_f(b, palette.accent[2], 0.75);
                        }
                    }
                }
            }

            // Subtle crisp 1px inner border frame
            if x == 0 || y == 0 || x + 1 == size || y + 1 == size {
                r = lerp_f(r, 255, 0.18);
                g = lerp_f(g, 255, 0.18);
                b = lerp_f(b, 255, 0.18);
            }

            pixels[(y * size + x) as usize] = Rgba8Pixel { r, g, b, a: 255 };
        }
    }

    Image::from_rgba8(buf)
}

fn lerp(a: u8, b: u8, t: f32) -> u8 {
    let t = t.clamp(0.0, 1.0);
    (a as f32 + (b as f32 - a as f32) * t).round() as u8
}

fn lerp_f(a: u8, b: u8, t: f32) -> u8 {
    lerp(a, b, t)
}

pub fn track_cover(index: usize) -> Image {
    const PALETTES: [CoverPalette; 7] = [
        // 0: Yorushika - deep twilight blue / seafoam cyan
        CoverPalette {
            bg_top: [22, 42, 78],
            bg_bottom: [14, 24, 46],
            accent: [76, 158, 232],
            highlight: [216, 238, 255],
            pattern: 0,
        },
        // 1: Miku x n-buna - deep sea teal / bioluminescent aqua
        CoverPalette {
            bg_top: [12, 62, 74],
            bg_bottom: [8, 28, 42],
            accent: [56, 206, 188],
            highlight: [190, 250, 242],
            pattern: 1,
        },
        // 2: Ryuichi Sakamoto - monochrome winter silver & slate
        CoverPalette {
            bg_top: [44, 50, 62],
            bg_bottom: [20, 24, 30],
            accent: [168, 180, 198],
            highlight: [238, 242, 248],
            pattern: 2,
        },
        // 3: Phantom Blade Zero - crimson ink & charcoal obsidian
        CoverPalette {
            bg_top: [54, 16, 22],
            bg_bottom: [18, 14, 18],
            accent: [214, 52, 62],
            highlight: [248, 204, 164],
            pattern: 3,
        },
        // 4: Disappearance of Miku - electric magenta & cyan cyber grid
        CoverPalette {
            bg_top: [32, 20, 56],
            bg_bottom: [16, 14, 32],
            accent: [54, 212, 224],
            highlight: [244, 92, 164],
            pattern: 3,
        },
        // 5: Nakiri Ayame - vermilion festival red & warm gold
        CoverPalette {
            bg_top: [92, 24, 34],
            bg_bottom: [34, 16, 28],
            accent: [236, 94, 88],
            highlight: [252, 214, 128],
            pattern: 0,
        },
        // 6: Rokudenashi - dusk lavender & warm coral
        CoverPalette {
            bg_top: [48, 38, 82],
            bg_bottom: [24, 22, 44],
            accent: [232, 124, 148],
            highlight: [246, 224, 212],
            pattern: 2,
        },
    ];
    make_cover(240, PALETTES[index % PALETTES.len()])
}

pub fn sample_tracks() -> Vec<TrackItem> {
    vec![
        TrackItem {
            id: 1,
            filename: "ヨルシカ - だから僕は音楽を辞めた.flac".into(),
            filedir: "~/Music".into(),
            tag_types: "FLAC (Vorbis)".into(),
            title: "だから僕は音楽を辞めた".into(),
            artist: "ヨルシカ".into(),
            album: "だから僕は音楽を辞めた".into(),
            album_artist: "ヨルシカ".into(),
            year: "2019".into(),
            track_str: "14".into(),
            disc_str: "1".into(),
            genre: "J-Pop / Rock".into(),
            composer: "n-buna".into(),
            lyricist: "n-buna".into(),
            comment: "163 key(Don't modify):L64FU3W4YxX3ZFTmbZ+8/Q==".into(),
            has_163_key: true,
            netease_id: "1357374736".into(),
            has_picture: true,
            cover_info: "3000×3000 · JPEG · 1.42 MB".into(),
            cover: track_cover(0),
            has_lyrics: true,
            lyrics_preview: "[00:01.08]考えたってわからないし / 想过之后依然搞不懂".into(),
            lyrics: "[00:01.08]考えたってわからないし\n[00:01.08]想过之后依然搞不懂\n[00:03.06]青空の下、君を待った\n[00:03.06]在蔚蓝的天空下等待着你\n[00:06.15]風が吹いた正午、昼下がりを抜け出す想像\n[00:06.15]微风吹拂的正午、趁着午后偷溜出去的想象\n[00:11.02]ねぇ、これからどうなるんだろうね\n[00:11.02]呐、接下来会变成怎样呢\n[00:14.40]進め方教わらないんだよ\n[00:14.40]没有人教过我该如何前行啊\n[00:17.85]あなたの言う通りお終いだ\n[00:17.85]正如你所说的那样到此为止了".into(),
            is_instrumental: false,
            rg_track_gain: "-8.63 dB".into(),
            rg_track_peak: "1.000000".into(),
            rg_album_gain: "-8.91 dB".into(),
            rg_album_peak: "1.000000".into(),
            bit_per_sample: "16 bit".into(),
            channels: "2".into(),
            sample_rate: "44100 Hz".into(),
            bitrate: "864 kbps".into(),
            duration: "04:02".into(),
            update_time: "2026-11-23 19:42:10".into(),
            modified: false,
            selected: true,
        },
        TrackItem {
            id: 2,
            filename: "初音ミク,n-buna - ウミユリ海底譚.flac".into(),
            filedir: "~/Music".into(),
            tag_types: "FLAC (Vorbis)".into(),
            title: "ウミユリ海底譚".into(),
            artist: "初音ミク;n-buna".into(),
            album: "「マジカルミライ 2016」OFFICIAL ALBUM".into(),
            album_artist: "Various Artists".into(),
            year: "2016".into(),
            track_str: "3".into(),
            disc_str: "1".into(),
            genre: "Vocaloid".into(),
            composer: "n-buna".into(),
            lyricist: "n-buna".into(),
            comment: "163 key(Don't modify):L64FU3W4YxX3ZFTmbZ+8/Q==".into(),
            has_163_key: true,
            netease_id: "425137083".into(),
            has_picture: true,
            cover_info: "1400×1400 · JPEG · 680 KB".into(),
            cover: track_cover(1),
            has_lyrics: true,
            lyrics_preview: "[00:02.23]待って　わかってよ / 等会啊 我知道的啊".into(),
            lyrics: "[00:02.23]待って　わかってよ\n[00:02.23]等会啊 我知道的啊\n[00:05.10]何でもないから僕の歌を笑わないで\n[00:05.10]没什么大不了的所以请不要嘲笑我的歌\n[00:10.44]空中散歩のＳＯＳ\n[00:10.44]空中散步的SOS\n[00:13.80]僕は　僕は　僕は\n[00:13.80]我啊 我啊 我啊".into(),
            is_instrumental: false,
            rg_track_gain: "-11.37 dB".into(),
            rg_track_peak: "1.000000".into(),
            rg_album_gain: "-10.84 dB".into(),
            rg_album_peak: "1.000000".into(),
            bit_per_sample: "16 bit".into(),
            channels: "2".into(),
            sample_rate: "44100 Hz".into(),
            bitrate: "1077 kbps".into(),
            duration: "04:00".into(),
            update_time: "2026-11-23 19:40:05".into(),
            modified: false,
            selected: false,
        },
        TrackItem {
            id: 3,
            filename: "坂本龍一 - Merry Christmas Mr. Lawrence.flac".into(),
            filedir: "~/Music".into(),
            tag_types: "FLAC (Vorbis)".into(),
            title: "Merry Christmas Mr. Lawrence".into(),
            artist: "坂本龍一".into(),
            album: "Merry Christmas Mr. Lawrence".into(),
            album_artist: "坂本龍一".into(),
            year: "1983".into(),
            track_str: "1".into(),
            disc_str: "1".into(),
            genre: "Soundtrack".into(),
            composer: "坂本龍一".into(),
            lyricist: "".into(),
            comment: "163 key(Don't modify):L64FU3W4YxX3ZFTmbZ+8/Q==".into(),
            has_163_key: true,
            netease_id: "4899371".into(),
            has_picture: true,
            cover_info: "1400×1400 · JPEG · 512 KB".into(),
            cover: track_cover(2),
            has_lyrics: false,
            lyrics_preview: "".into(),
            lyrics: "".into(),
            is_instrumental: true,
            rg_track_gain: "-5.42 dB".into(),
            rg_track_peak: "0.971204".into(),
            rg_album_gain: "-6.10 dB".into(),
            rg_album_peak: "0.971204".into(),
            bit_per_sample: "24 bit".into(),
            channels: "2".into(),
            sample_rate: "44100 Hz".into(),
            bitrate: "1405 kbps".into(),
            duration: "04:36".into(),
            update_time: "2026-11-23 19:38:22".into(),
            modified: false,
            selected: false,
        },
        TrackItem {
            id: 4,
            filename: "薄彩生 - 赤黑传承.flac".into(),
            filedir: "~/Music".into(),
            tag_types: "FLAC (Vorbis)".into(),
            title: "赤黑传承".into(),
            artist: "薄彩生".into(),
            album: "影之刃零原声曲辑".into(),
            album_artist: "薄彩生".into(),
            year: "2026".into(),
            track_str: "14".into(),
            disc_str: "1".into(),
            genre: "Game Soundtrack".into(),
            composer: "薄彩生".into(),
            lyricist: "".into(),
            comment: "163 key(Don't modify):L64FU3W4YxX3ZFTmbZ+8/Q==".into(),
            has_163_key: true,
            netease_id: "2619048215".into(),
            has_picture: true,
            cover_info: "3000×3000 · JPEG · 1.85 MB".into(),
            cover: track_cover(3),
            has_lyrics: false,
            lyrics_preview: "".into(),
            lyrics: "".into(),
            is_instrumental: true,
            rg_track_gain: "-9.83 dB".into(),
            rg_track_peak: "0.988541".into(),
            rg_album_gain: "-9.50 dB".into(),
            rg_album_peak: "0.988541".into(),
            bit_per_sample: "24 bit".into(),
            channels: "2".into(),
            sample_rate: "44100 Hz".into(),
            bitrate: "1520 kbps".into(),
            duration: "03:48".into(),
            update_time: "2026-11-23 19:35:19".into(),
            modified: false,
            selected: false,
        },
        TrackItem {
            id: 5,
            filename: "初音ミク,cosMo@暴走P - 初音未来的消失.mp3".into(),
            filedir: "~/Music".into(),
            tag_types: "ID3v2.4".into(),
            title: "初音未来的消失".into(),
            artist: "初音ミク;cosMo@暴走P".into(),
            album: "《初音速》游戏原声音乐".into(),
            album_artist: "cosMo@暴走P".into(),
            year: "2017".into(),
            track_str: "2".into(),
            disc_str: "1".into(),
            genre: "Vocaloid".into(),
            composer: "cosMo@暴走P".into(),
            lyricist: "cosMo@暴走P".into(),
            comment: "163 key(Don't modify):L64FU3W4YxX3ZFTmbZ+8/Q==".into(),
            has_163_key: true,
            netease_id: "526468901".into(),
            has_picture: true,
            cover_info: "1920×1920 · JPEG · 920 KB".into(),
            cover: track_cover(4),
            has_lyrics: true,
            lyrics_preview: "[00:00.50]ボクは生まれ そして気づく / 我诞生于此 随后察觉到".into(),
            lyrics: "[00:00.50]ボクは生まれ そして気づく\n[00:00.50]我诞生于此 随后察觉到\n[00:04.20]所詮 ヒトの真似事だと\n[00:04.20]归根结底 不过是人类的模仿品\n[00:08.15]知ってなおも歌い続く\n[00:08.15]即便知道如此 仍旧继续歌唱".into(),
            is_instrumental: false,
            rg_track_gain: "-12.29 dB".into(),
            rg_track_peak: "1.067152".into(),
            rg_album_gain: "-11.95 dB".into(),
            rg_album_peak: "1.067152".into(),
            bit_per_sample: "16 bit".into(),
            channels: "2".into(),
            sample_rate: "44100 Hz".into(),
            bitrate: "332 kbps".into(),
            duration: "04:46".into(),
            update_time: "2026-11-23 19:33:44".into(),
            modified: false,
            selected: false,
        },
        TrackItem {
            id: 6,
            filename: "百鬼あやめ - 夢花火.mp3".into(),
            filedir: "~/Music".into(),
            tag_types: "ID3v2.4".into(),
            title: "夢花火".into(),
            artist: "百鬼あやめ".into(),
            album: "夢花火".into(),
            album_artist: "百鬼あやめ".into(),
            year: "2022".into(),
            track_str: "1".into(),
            disc_str: "01".into(),
            genre: "J-Pop".into(),
            composer: "白戸佑輔".into(),
            lyricist: "白戸佑輔".into(),
            comment: "163 key(Don't modify):L64FU3W4YxX3ZFTmbZ+8/Q==".into(),
            has_163_key: true,
            netease_id: "1987654321".into(),
            has_picture: true,
            cover_info: "3000×3000 · JPEG · 1.64 MB".into(),
            cover: track_cover(5),
            has_lyrics: true,
            lyrics_preview: "[00:01.40]揺れる水面 映る月の影 / 摇曳的水面 倒映着月亮的影子".into(),
            lyrics: "[00:01.40]揺れる水面 映る月の影\n[00:01.40]摇曳的水面 倒映着月亮的影子\n[00:06.12]祭囃子が遠く響いて\n[00:06.12]祭典的笛鼓声在远方回响\n[00:11.05]打ち上がる夢花火\n[00:11.05]升上夜空的梦花火".into(),
            is_instrumental: false,
            rg_track_gain: "-10.59 dB".into(),
            rg_track_peak: "1.076818".into(),
            rg_album_gain: "-10.59 dB".into(),
            rg_album_peak: "1.076818".into(),
            bit_per_sample: "16 bit".into(),
            channels: "2".into(),
            sample_rate: "44100 Hz".into(),
            bitrate: "364 kbps".into(),
            duration: "04:00".into(),
            update_time: "2026-11-23 19:30:11".into(),
            modified: false,
            selected: false,
        },
        TrackItem {
            id: 7,
            filename: "ロクデナシ - ただ声一つ.flac".into(),
            filedir: "~/Music".into(),
            tag_types: "FLAC (Vorbis)".into(),
            title: "ただ声一つ".into(),
            artist: "ロクデナシ".into(),
            album: "ただ声一つ".into(),
            album_artist: "ロクデナシ".into(),
            year: "2021".into(),
            track_str: "1".into(),
            disc_str: "1".into(),
            genre: "J-Pop".into(),
            composer: "MIMI".into(),
            lyricist: "MIMI".into(),
            comment: "163 key(Don't modify):L64FU3W4YxX3ZFTmbZ+8/Q==".into(),
            has_163_key: true,
            netease_id: "1905310150".into(),
            has_picture: true,
            cover_info: "1500×1500 · PNG · 1.12 MB".into(),
            cover: track_cover(6),
            has_lyrics: true,
            lyrics_preview: "[00:00.85]続く時間の針を眺めて / 凝望着不断走动的时针".into(),
            lyrics: "[00:00.85]続く時間の針を眺めて\n[00:00.85]凝望着不断走动的时针\n[00:04.90]ただ声一つ それだけで\n[00:04.90]仅仅凭藉这一缕声音".into(),
            is_instrumental: false,
            rg_track_gain: "-7.91 dB".into(),
            rg_track_peak: "0.992100".into(),
            rg_album_gain: "-7.91 dB".into(),
            rg_album_peak: "0.992100".into(),
            bit_per_sample: "24 bit".into(),
            channels: "2".into(),
            sample_rate: "48000 Hz".into(),
            bitrate: "1642 kbps".into(),
            duration: "02:41".into(),
            update_time: "2026-11-23 19:28:50".into(),
            modified: true,
            selected: false,
        },
        TrackItem {
            id: 8,
            filename: "3R2 - New Quest.flac".into(),
            filedir: "~/Music".into(),
            tag_types: "FLAC (Vorbis)".into(),
            title: "New Quest".into(),
            artist: "3R2".into(),
            album: "".into(),
            album_artist: "".into(),
            year: "".into(),
            track_str: "".into(),
            disc_str: "".into(),
            genre: "Electronic".into(),
            composer: "".into(),
            lyricist: "".into(),
            comment: "".into(),
            has_163_key: false,
            netease_id: "".into(),
            has_picture: false,
            cover_info: "".into(),
            cover: Image::default(),
            has_lyrics: false,
            lyrics_preview: "".into(),
            lyrics: "".into(),
            is_instrumental: false,
            rg_track_gain: "".into(),
            rg_track_peak: "".into(),
            rg_album_gain: "".into(),
            rg_album_peak: "".into(),
            bit_per_sample: "16 bit".into(),
            channels: "2".into(),
            sample_rate: "44100 Hz".into(),
            bitrate: "945 kbps".into(),
            duration: "03:18".into(),
            update_time: "2026-11-23 19:20:04".into(),
            modified: false,
            selected: false,
        },
    ]
}

pub fn sample_search_candidates() -> Vec<SearchCandidate> {
    vec![
        SearchCandidate {
            song_id: "1357374736".into(),
            title: "だから僕は音楽を辞めた".into(),
            artist: "ヨルシカ".into(),
            album: "だから僕は音楽を辞めた".into(),
            year: "2019".into(),
            duration: "04:02".into(),
            source: "网易云音乐".into(),
            cover_size: "3000×3000".into(),
            cover: track_cover(0),
            has_translation: true,
            is_instrumental: false,
            lyric_preview: "[00:01.08]考えたってわからないし\n[00:01.08]想过之后依然搞不懂\n[00:03.06]青空の下、君を待った\n[00:03.06]在蔚蓝的天空下等待着你".into(),
        },
        SearchCandidate {
            song_id: "1407551413".into(),
            title: "だから僕は音楽を辞めた (Live at Zepp)".into(),
            artist: "ヨルシカ".into(),
            album: "月光".into(),
            year: "2022".into(),
            duration: "04:08".into(),
            source: "QQ音乐".into(),
            cover_size: "1400×1400".into(),
            cover: track_cover(1),
            has_translation: true,
            is_instrumental: false,
            lyric_preview: "[00:02.40]考えたってわからないし\n[00:02.40]想过之后依然搞不懂".into(),
        },
        SearchCandidate {
            song_id: "1357374799".into(),
            title: "だから僕は音楽を辞めた (Instrumental)".into(),
            artist: "ヨルシカ".into(),
            album: "だから僕は音楽を辞めた (Bonus)".into(),
            year: "2019".into(),
            duration: "04:02".into(),
            source: "网易云音乐".into(),
            cover_size: "1000×1000".into(),
            cover: track_cover(2),
            has_translation: false,
            is_instrumental: true,
            lyric_preview: "(纯音乐 · 无歌词)".into(),
        },
        SearchCandidate {
            song_id: "1462091124".into(),
            title: "だから僕は音楽を辞めた".into(),
            artist: "ヨルシカ".into(),
            album: "だから僕は音楽を辞めた [Hi-Res]".into(),
            year: "2019".into(),
            duration: "04:02".into(),
            source: "iTunes".into(),
            cover_size: "3000×3000".into(),
            cover: track_cover(6),
            has_translation: false,
            is_instrumental: false,
            lyric_preview: "[00:01.08]考えたってわからないし\n[00:03.06]青空の下、君を待った".into(),
        },
    ]
}

pub fn sample_cover_candidates() -> Vec<CoverCandidate> {
    vec![
        CoverCandidate {
            source: "网易云音乐".into(),
            resolution: "3000×3000".into(),
            size_kb: "1452 KB".into(),
            format: "JPEG".into(),
            title: "だから僕は音楽を辞めた (初回限定盤)".into(),
            cover: track_cover(0),
        },
        CoverCandidate {
            source: "iTunes".into(),
            resolution: "3000×3000".into(),
            size_kb: "1890 KB".into(),
            format: "PNG".into(),
            title: "だから僕は音楽を辞めた (Digital)".into(),
            cover: track_cover(1),
        },
        CoverCandidate {
            source: "QQ音乐".into(),
            resolution: "1400×1400".into(),
            size_kb: "640 KB".into(),
            format: "JPEG".into(),
            title: "だから僕は音楽を辞めた (Standard)".into(),
            cover: track_cover(5),
        },
        CoverCandidate {
            source: "Discogs".into(),
            resolution: "1000×1000".into(),
            size_kb: "412 KB".into(),
            format: "JPEG".into(),
            title: "Yorushika - Elma / Dakara Boku".into(),
            cover: track_cover(6),
        },
    ]
}

pub fn default_columns() -> Vec<ColumnConfigItem> {
    vec![
        ColumnConfigItem { key: "filename".into(), label: "文件名".into(), visible: true, width: 250 },
        ColumnConfigItem { key: "title".into(), label: "标题".into(), visible: true, width: 170 },
        ColumnConfigItem { key: "artist".into(), label: "艺术家".into(), visible: true, width: 140 },
        ColumnConfigItem { key: "album".into(), label: "专辑".into(), visible: true, width: 170 },
        ColumnConfigItem { key: "albumartist".into(), label: "专辑艺术家".into(), visible: true, width: 120 },
        ColumnConfigItem { key: "year".into(), label: "年份".into(), visible: true, width: 64 },
        ColumnConfigItem { key: "trackstr".into(), label: "音轨".into(), visible: true, width: 56 },
        ColumnConfigItem { key: "discstr".into(), label: "碟号".into(), visible: true, width: 56 },
        ColumnConfigItem { key: "genre".into(), label: "流派".into(), visible: true, width: 100 },
        ColumnConfigItem { key: "composer".into(), label: "作曲".into(), visible: false, width: 100 },
        ColumnConfigItem { key: "lyricist".into(), label: "作词".into(), visible: false, width: 100 },
        ColumnConfigItem { key: "comment".into(), label: "注释".into(), visible: false, width: 130 },
        ColumnConfigItem { key: "haspicture".into(), label: "封面".into(), visible: true, width: 68 },
        ColumnConfigItem { key: "lyrics".into(), label: "歌词".into(), visible: true, width: 68 },
        ColumnConfigItem { key: "rg".into(), label: "回放增益".into(), visible: true, width: 92 },
        ColumnConfigItem { key: "bitpersample".into(), label: "位深".into(), visible: true, width: 64 },
        ColumnConfigItem { key: "channels".into(), label: "声道".into(), visible: false, width: 56 },
        ColumnConfigItem { key: "samplerate".into(), label: "采样率".into(), visible: true, width: 82 },
        ColumnConfigItem { key: "bitrate".into(), label: "比特率".into(), visible: true, width: 82 },
        ColumnConfigItem { key: "duration".into(), label: "时长".into(), visible: true, width: 68 },
        ColumnConfigItem { key: "updatetime".into(), label: "修改时间".into(), visible: false, width: 148 },
        ColumnConfigItem { key: "filedir".into(), label: "目录".into(), visible: false, width: 180 },
        ColumnConfigItem { key: "tagtypes".into(), label: "标签格式".into(), visible: true, width: 108 },
    ]
}

pub fn sample_dirs() -> Vec<DirEntry> {
    vec![
        DirEntry { path: "~/Music".into(), include_subdirs: true, file_count: 8 },
        DirEntry { path: "~/Music/Vocaloid-Lossless".into(), include_subdirs: true, file_count: 42 },
        DirEntry { path: "~/Downloads/HiRes".into(), include_subdirs: false, file_count: 11 },
    ]
}

pub fn sample_history() -> Vec<HistoryEntry> {
    vec![
        HistoryEntry {
            timestamp: "2026-11-23 19:42:10".into(),
            summary: "网易云组合标签匹配 (标题/专辑/封面/双语歌词/163 key)".into(),
            title: "だから僕は音楽を辞めた".into(),
            artist: "ヨルシカ".into(),
            album: "だから僕は音楽を辞めた".into(),
            comment_has_163: true,
        },
        HistoryEntry {
            timestamp: "2026-11-23 19:40:05".into(),
            summary: "保存双语歌词 + 写入 ReplayGain (-8.63 dB)".into(),
            title: "だから僕は音楽を辞めた".into(),
            artist: "ヨルシカ".into(),
            album: "だから僕は音楽を辞めた".into(),
            comment_has_163: true,
        },
        HistoryEntry {
            timestamp: "2026-11-22 22:18:33".into(),
            summary: "从文件名解析标签 (艺术家 - 标题)".into(),
            title: "だから僕は音楽を辞めた".into(),
            artist: "ヨルシカ".into(),
            album: "".into(),
            comment_has_163: false,
        },
        HistoryEntry {
            timestamp: "2026-11-21 15:04:12".into(),
            summary: "初始读取 FLAC Vorbis Comment".into(),
            title: "Dakara Boku wa Ongaku wo Yameta".into(),
            artist: "Yorushika".into(),
            album: "Dakara Boku".into(),
            comment_has_163: false,
        },
    ]
}

pub fn sample_regex_groups() -> Vec<RegexGroupItem> {
    vec![
        RegexGroupItem { group_index: 1, target_field: "@5 - 音轨号".into() },
        RegexGroupItem { group_index: 2, target_field: "@2 - 艺术家".into() },
        RegexGroupItem { group_index: 3, target_field: "@1 - 标题".into() },
    ]
}

pub fn sample_filename_previews() -> Vec<FilenamePreviewItem> {
    vec![
        FilenamePreviewItem {
            before: "ヨルシカ - だから僕は音楽を辞めた.flac".into(),
            after: "14. ヨルシカ - だから僕は音楽を辞めた.flac".into(),
            status: "就绪".into(),
        },
        FilenamePreviewItem {
            before: "初音ミク,n-buna - ウミユリ海底譚.flac".into(),
            after: "03. 初音ミク;n-buna - ウミユリ海底譚.flac".into(),
            status: "就绪".into(),
        },
        FilenamePreviewItem {
            before: "坂本龍一 - Merry Christmas Mr. Lawrence.flac".into(),
            after: "01. 坂本龍一 - Merry Christmas Mr. Lawrence.flac".into(),
            status: "就绪".into(),
        },
        FilenamePreviewItem {
            before: "3R2 - New Quest.flac".into(),
            after: "3R2 - New Quest.flac".into(),
            status: "跳过 (缺少音轨号)".into(),
        },
    ]
}

pub fn sample_auto_match_rows() -> Vec<AutoMatchRow> {
    vec![
        AutoMatchRow {
            filename: "ヨルシカ - だから僕は音楽を辞めた.flac".into(),
            matched_title: "だから僕は音楽を辞めた".into(),
            matched_artist: "ヨルシカ".into(),
            status: "已匹配".into(),
            detail: "网易云 #1357374736 · 封面 3000×3000 · 双语歌词".into(),
        },
        AutoMatchRow {
            filename: "初音ミク,n-buna - ウミユリ海底譚.flac".into(),
            matched_title: "ウミユリ海底譚".into(),
            matched_artist: "初音ミク;n-buna".into(),
            status: "已匹配".into(),
            detail: "网易云 #425137083 · 封面 1400×1400 · 双语歌词".into(),
        },
        AutoMatchRow {
            filename: "坂本龍一 - Merry Christmas Mr. Lawrence.flac".into(),
            matched_title: "Merry Christmas Mr. Lawrence".into(),
            matched_artist: "坂本龍一".into(),
            status: "已匹配 (纯音乐)".into(),
            detail: "保留标签与封面 · 跳过歌词下载".into(),
        },
        AutoMatchRow {
            filename: "ロクデナシ - ただ声一つ.flac".into(),
            matched_title: "ただ声一つ".into(),
            matched_artist: "ロクデナシ".into(),
            status: "已匹配".into(),
            detail: "网易云 #1905310150 · 封面 1500×1500 · 双语歌词".into(),
        },
        AutoMatchRow {
            filename: "3R2 - New Quest.flac".into(),
            matched_title: "".into(),
            matched_artist: "".into(),
            status: "未找到".into(),
            detail: "无精确匹配候选，请手动组合搜索".into(),
        },
    ]
}

pub fn sample_charset_rows() -> Vec<CharsetPreviewRow> {
    vec![
        CharsetPreviewRow {
            field: "标题".into(),
            original: "Ó°Ö®ÈÐÁã - ³àºÚ´«³Ð".into(),
            converted: "影之刃零 - 赤黑传承".into(),
        },
        CharsetPreviewRow {
            field: "艺术家".into(),
            original: "±¡²ÊÉú".into(),
            converted: "薄彩生".into(),
        },
        CharsetPreviewRow {
            field: "专辑".into(),
            original: "Ó°Ö®ÈÐÁãÔ­ÉùÇú¼­".into(),
            converted: "影之刃零原声曲辑".into(),
        },
        CharsetPreviewRow {
            field: "注释".into(),
            original: "ÍøÒ×ÔÆÒôÀÖ±êÇ©".into(),
            converted: "网易云音乐标签".into(),
        },
    ]
}
