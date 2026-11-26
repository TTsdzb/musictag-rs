// Prevent console window in addition to Slint window in Windows release builds when, e.g., starting the app via file manager. Ignored on other platforms.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::cell::{Cell, RefCell};
use std::error::Error;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Platform, WindowAdapter};
use slint::{ComponentHandle, Model, ModelRc, PhysicalSize, SharedString, VecModel};

mod backend_controller;
mod sample_data;

slint::include_modules!();

const TEXT_FIELDS: &[&str] = &[
    "title",
    "artist",
    "album",
    "album-artist",
    "year",
    "track-str",
    "disc-str",
    "genre",
    "composer",
    "lyricist",
    "comment",
];

fn field_index(field: &str) -> Option<usize> {
    match field {
        "title" => Some(0),
        "artist" => Some(1),
        "album" => Some(2),
        "album-artist" => Some(3),
        "year" => Some(4),
        "track-str" => Some(5),
        "disc-str" => Some(6),
        "genre" => Some(7),
        "composer" => Some(8),
        "lyricist" => Some(9),
        "comment" => Some(10),
        "cover" => Some(11),
        "lyrics" => Some(12),
        _ => None,
    }
}

fn track_text_field(track: &TrackItem, field: &str) -> SharedString {
    match field {
        "title" => track.title.clone(),
        "artist" => track.artist.clone(),
        "album" => track.album.clone(),
        "album-artist" => track.album_artist.clone(),
        "year" => track.year.clone(),
        "track-str" => track.track_str.clone(),
        "disc-str" => track.disc_str.clone(),
        "genre" => track.genre.clone(),
        "composer" => track.composer.clone(),
        "lyricist" => track.lyricist.clone(),
        "comment" => track.comment.clone(),
        _ => SharedString::default(),
    }
}

fn set_track_text_field(track: &mut TrackItem, field: &str, value: SharedString) {
    match field {
        "title" => track.title = value,
        "artist" => track.artist = value,
        "album" => track.album = value,
        "album-artist" => track.album_artist = value,
        "year" => track.year = value,
        "track-str" => track.track_str = value,
        "disc-str" => track.disc_str = value,
        "genre" => track.genre = value,
        "composer" => track.composer = value,
        "lyricist" => track.lyricist = value,
        "comment" => {
            track.has_163_key = value.contains("163 key");
            track.comment = value;
        }
        _ => {}
    }
}

fn restore_track_field(track: &mut TrackItem, orig: &TrackItem, field: &str) {
    match field {
        "cover" => {
            track.cover = orig.cover.clone();
            track.has_picture = orig.has_picture;
            track.cover_info = orig.cover_info.clone();
        }
        "lyrics" => {
            track.lyrics = orig.lyrics.clone();
            track.has_lyrics = orig.has_lyrics;
            track.lyrics_preview = orig.lyrics_preview.clone();
            track.is_instrumental = orig.is_instrumental;
        }
        "comment" => {
            track.comment = orig.comment.clone();
            track.has_163_key = orig.has_163_key;
            track.netease_id = orig.netease_id.clone();
        }
        _ => set_track_text_field(track, field, track_text_field(orig, field)),
    }
}

fn clear_track_field(track: &mut TrackItem, field: &str) {
    match field {
        "cover" => {
            track.cover = slint::Image::default();
            track.has_picture = false;
            track.cover_info = "".into();
        }
        "lyrics" => {
            track.lyrics = "".into();
            track.has_lyrics = false;
            track.lyrics_preview = "".into();
            track.is_instrumental = false;
        }
        "comment" => {
            track.comment = "".into();
            track.has_163_key = false;
            track.netease_id = "".into();
        }
        _ => set_track_text_field(track, field, "".into()),
    }
}

fn set_inspector_field(state: &AppState<'_>, field: &str, value: InspectorField) {
    match field {
        "title" => state.set_field_title(value),
        "artist" => state.set_field_artist(value),
        "album" => state.set_field_album(value),
        "album-artist" => state.set_field_album_artist(value),
        "year" => state.set_field_year(value),
        "track-str" => state.set_field_track(value),
        "disc-str" => state.set_field_disc(value),
        "genre" => state.set_field_genre(value),
        "composer" => state.set_field_composer(value),
        "lyricist" => state.set_field_lyricist(value),
        "comment" => state.set_field_comment(value),
        _ => {}
    }
}

struct HeadlessPlatform {
    window: Rc<MinimalSoftwareWindow>,
    clock: Rc<Cell<Duration>>,
}

impl Platform for HeadlessPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.window.clone())
    }

    fn duration_since_start(&self) -> Duration {
        self.clock.get()
    }
}

struct AppController {
    original_tracks: Vec<TrackItem>,
    master_tracks: Vec<TrackItem>,
    initial_modified: Vec<bool>,
    other_modified: Vec<bool>,
    field_modes: Vec<[i32; 13]>,
    filtered_indices: Vec<usize>,
    tracks_model: Rc<VecModel<TrackItem>>,
    candidates_model: Rc<VecModel<SearchCandidate>>,
    covers_model: Rc<VecModel<CoverCandidate>>,
    columns_model: Rc<VecModel<ColumnConfigItem>>,
    dirs_model: Rc<VecModel<DirEntry>>,
    history_model: Rc<VecModel<HistoryEntry>>,
    regex_groups_model: Rc<VecModel<RegexGroupItem>>,
    filename_previews_model: Rc<VecModel<FilenamePreviewItem>>,
    auto_match_model: Rc<VecModel<AutoMatchRow>>,
    charset_model: Rc<VecModel<CharsetPreviewRow>>,
    sources_model: Rc<VecModel<SourceItem>>,
}

fn sample_source_items() -> Vec<SourceItem> {
    vec![
        SourceItem {
            id: "netease".into(),
            name: "网易云音乐".into(),
            version: "1.0.0".into(),
            description: "NetEase Cloud Music metadata, lyrics, high-res covers, and 163 key".into(),
            builtin: true,
            enabled: true,
            supports_combo: true,
            supports_lyrics: true,
            supports_cover: true,
            supports_163: true,
        },
        SourceItem {
            id: "example-custom-js".into(),
            name: "自定义 JS 插件示例".into(),
            version: "1.0.0".into(),
            description: "MusicTag Native QuickJS 音源插件开发模板（演示 MusicTagHost.httpGet、搜索、详情、歌词与封面接口）".into(),
            builtin: false,
            enabled: false,
            supports_combo: true,
            supports_lyrics: true,
            supports_cover: true,
            supports_163: false,
        },
    ]
}

impl AppController {
    fn new() -> Self {
        let master_tracks = sample_data::sample_tracks();
        let original_tracks = master_tracks.clone();
        let initial_modified: Vec<bool> = master_tracks.iter().map(|t| t.modified).collect();
        let other_modified = vec![false; master_tracks.len()];
        let field_modes = vec![[1; 13]; master_tracks.len()];
        let filtered_indices: Vec<usize> = (0..master_tracks.len()).collect();
        let tracks_model = Rc::new(VecModel::from(master_tracks.clone()));
        Self {
            original_tracks,
            master_tracks,
            initial_modified,
            other_modified,
            field_modes,
            filtered_indices,
            tracks_model,
            candidates_model: Rc::new(VecModel::from(sample_data::sample_search_candidates())),
            covers_model: Rc::new(VecModel::from(sample_data::sample_cover_candidates())),
            columns_model: Rc::new(VecModel::from(sample_data::default_columns())),
            dirs_model: Rc::new(VecModel::from(sample_data::sample_dirs())),
            history_model: Rc::new(VecModel::from(sample_data::sample_history())),
            regex_groups_model: Rc::new(VecModel::from(sample_data::sample_regex_groups())),
            filename_previews_model: Rc::new(VecModel::from(
                sample_data::sample_filename_previews(),
            )),
            auto_match_model: Rc::new(VecModel::from(sample_data::sample_auto_match_rows())),
            charset_model: Rc::new(VecModel::from(sample_data::sample_charset_rows())),
            sources_model: Rc::new(VecModel::from(sample_source_items())),
        }
    }

    fn selected_master_index(&self, ui: &AppWindow) -> Option<usize> {
        let idx = ui.global::<AppState>().get_selected_index();
        if idx < 0 {
            return None;
        }
        self.filtered_indices.get(idx as usize).copied()
    }

    fn checked_master_indices(&self) -> Vec<usize> {
        self.master_tracks
            .iter()
            .enumerate()
            .filter(|(_, t)| t.selected)
            .map(|(i, _)| i)
            .collect()
    }

    fn target_master_indices(&self, ui: &AppWindow) -> Vec<usize> {
        let checked = self.checked_master_indices();
        if checked.len() > 1 {
            checked
        } else {
            self.selected_master_index(ui).into_iter().collect()
        }
    }

    fn recompute_track_modified(&mut self, m_idx: usize) {
        self.master_tracks[m_idx].modified = self.initial_modified[m_idx]
            || self.other_modified[m_idx]
            || self.field_modes[m_idx].iter().any(|&m| m != 1);
    }

    fn sync_visible_tracks(&self) {
        for (v_idx, &m_idx) in self.filtered_indices.iter().enumerate() {
            if v_idx < self.tracks_model.row_count() {
                self.tracks_model
                    .set_row_data(v_idx, self.master_tracks[m_idx].clone());
            }
        }
    }

    fn sync_counts_and_status(&self, ui: &AppWindow, extra: Option<&str>) {
        let state = ui.global::<AppState>();
        let selected_count = self.master_tracks.iter().filter(|t| t.selected).count() as i32;
        let modified_count = self.master_tracks.iter().filter(|t| t.modified).count() as i32;
        state.set_selected_count(selected_count);
        state.set_modified_count(modified_count);

        let visible_total = self.filtered_indices.len();
        let total = self.master_tracks.len();
        let base = if visible_total == total {
            format!(
                "共 {total} 首曲目 · 已选中 {selected_count} 首 · {modified_count} 首待保存"
            )
        } else {
            format!(
                "筛选 {visible_total}/{total} 首 · 已选中 {selected_count} 首 · {modified_count} 首待保存"
            )
        };
        let msg = match extra {
            Some(note) if !note.is_empty() => format!("{base} · {note}"),
            _ => base,
        };
        state.set_status_message(msg.into());
    }

    fn sync_selected_track_inputs(&self, ui: &AppWindow) {
        let state = ui.global::<AppState>();
        let checked = self.checked_master_indices();

        if let Some(m_idx) = self.selected_master_index(ui) {
            let track = &self.master_tracks[m_idx];
            state.set_combo_query_title(track.title.clone());
            state.set_combo_query_artist(track.artist.clone());
            state.set_combo_query_album(track.album.clone());
            state.set_combo_query_id(track.netease_id.clone());
            state.set_lyric_editor_text(track.lyrics.clone());
        }

        if checked.len() > 1 {
            state.set_is_multi_select(true);

            for &field in TEXT_FIELDS {
                let f_idx = field_index(field).unwrap();
                let first_m = self.field_modes[checked[0]][f_idx];
                let first_val = track_text_field(&self.master_tracks[checked[0]], field);
                let all_same_mode = checked.iter().all(|&i| self.field_modes[i][f_idx] == first_m);
                let all_same_val = checked
                    .iter()
                    .all(|&i| track_text_field(&self.master_tracks[i], field) == first_val);

                let insp = if all_same_mode && first_m == 2 {
                    InspectorField {
                        text: "".into(),
                        mode: 2,
                        mixed: false,
                    }
                } else if all_same_val {
                    let mode = if all_same_mode && first_m == 0 { 0 } else { 1 };
                    InspectorField {
                        text: first_val,
                        mode,
                        mixed: false,
                    }
                } else {
                    InspectorField {
                        text: "".into(),
                        mode: 1,
                        mixed: true,
                    }
                };
                set_inspector_field(&state, field, insp);
            }

            let first_t = &self.master_tracks[checked[0]];
            let all_clear_cover = checked.iter().all(|&i| self.field_modes[i][11] == 2);
            let all_same_cover = checked.iter().all(|&i| {
                let t = &self.master_tracks[i];
                t.has_picture == first_t.has_picture
                    && t.cover_info == first_t.cover_info
                    && t.cover == first_t.cover
            });
            if all_clear_cover {
                state.set_field_cover_mode(2);
                state.set_field_cover_mixed(false);
            } else {
                let all_val_cover = checked.iter().all(|&i| self.field_modes[i][11] == 0);
                state.set_field_cover_mode(if all_val_cover && all_same_cover { 0 } else { 1 });
                state.set_field_cover_mixed(!all_same_cover);
            }

            let all_clear_lyrics = checked.iter().all(|&i| self.field_modes[i][12] == 2);
            let all_same_lyrics = checked.iter().all(|&i| {
                let t = &self.master_tracks[i];
                t.has_lyrics == first_t.has_lyrics
                    && t.is_instrumental == first_t.is_instrumental
                    && t.lyrics == first_t.lyrics
            });
            if all_clear_lyrics {
                state.set_field_lyrics_mode(2);
                state.set_field_lyrics_mixed(false);
            } else {
                let all_val_lyrics = checked.iter().all(|&i| self.field_modes[i][12] == 0);
                state.set_field_lyrics_mode(if all_val_lyrics && all_same_lyrics { 0 } else { 1 });
                state.set_field_lyrics_mixed(!all_same_lyrics);
            }
        } else {
            state.set_is_multi_select(false);

            if let Some(m_idx) = self.selected_master_index(ui) {
                let track = &self.master_tracks[m_idx];
                let orig = &self.original_tracks[m_idx];

                for &field in TEXT_FIELDS {
                    let f_idx = field_index(field).unwrap();
                    let tracked_mode = self.field_modes[m_idx][f_idx];
                    let cur_val = track_text_field(track, field);
                    let orig_val = track_text_field(orig, field);
                    let mode = if tracked_mode == 2 {
                        2
                    } else if cur_val != orig_val || tracked_mode == 0 {
                        0
                    } else {
                        1
                    };
                    set_inspector_field(
                        &state,
                        field,
                        InspectorField {
                            text: cur_val,
                            mode,
                            mixed: false,
                        },
                    );
                }

                let cover_mode = if self.field_modes[m_idx][11] == 2 {
                    2
                } else if track.has_picture != orig.has_picture
                    || track.cover_info != orig.cover_info
                    || track.cover != orig.cover
                    || self.field_modes[m_idx][11] == 0
                {
                    0
                } else {
                    1
                };
                state.set_field_cover_mode(cover_mode);
                state.set_field_cover_mixed(false);

                let lyrics_mode = if self.field_modes[m_idx][12] == 2 {
                    2
                } else if track.lyrics != orig.lyrics
                    || track.has_lyrics != orig.has_lyrics
                    || track.is_instrumental != orig.is_instrumental
                    || self.field_modes[m_idx][12] == 0
                {
                    0
                } else {
                    1
                };
                state.set_field_lyrics_mode(lyrics_mode);
                state.set_field_lyrics_mixed(false);
            } else {
                for &field in TEXT_FIELDS {
                    set_inspector_field(
                        &state,
                        field,
                        InspectorField {
                            text: "".into(),
                            mode: 1,
                            mixed: false,
                        },
                    );
                }
                state.set_field_cover_mode(1);
                state.set_field_cover_mixed(false);
                state.set_field_lyrics_mode(1);
                state.set_field_lyrics_mixed(false);
            }
        }
    }

    fn update_visible_track(&self, visible_idx: usize, track: TrackItem) {
        if visible_idx < self.tracks_model.row_count() {
            self.tracks_model.set_row_data(visible_idx, track);
        }
    }

    fn rebuild_filtered_tracks(&mut self, ui: &AppWindow, field_idx: i32, keyword: &str) {
        let kw = keyword.trim().to_lowercase();
        self.filtered_indices.clear();
        let mut visible = Vec::new();

        for (i, t) in self.master_tracks.iter().enumerate() {
            let matched = if kw.is_empty() {
                true
            } else {
                let contains = |s: &SharedString| s.to_lowercase().contains(&kw);
                match field_idx {
                    1 => contains(&t.filename),
                    2 => contains(&t.title),
                    3 => contains(&t.artist) || contains(&t.album_artist),
                    4 => contains(&t.album),
                    5 => contains(&t.tag_types),
                    6 => contains(&t.genre) || contains(&t.year),
                    _ => {
                        contains(&t.filename)
                            || contains(&t.title)
                            || contains(&t.artist)
                            || contains(&t.album)
                            || contains(&t.genre)
                            || contains(&t.netease_id)
                    }
                }
            };
            if matched {
                self.filtered_indices.push(i);
                visible.push(t.clone());
            }
        }

        self.tracks_model.set_vec(visible);
        let state = ui.global::<AppState>();
        state.set_filter_field_index(field_idx);
        state.set_filter_keyword(keyword.into());
        if self.filtered_indices.is_empty() {
            state.set_selected_index(-1);
        } else {
            let cur = state.get_selected_index();
            if cur < 0 || (cur as usize) >= self.filtered_indices.len() {
                state.set_selected_index(0);
            }
        }
        self.sync_selected_track_inputs(ui);
        self.sync_counts_and_status(ui, None);
    }
}

fn apply_edit_field(ui: &AppWindow, c: &mut AppController, field: &str, value: SharedString) {
    let targets = c.target_master_indices(ui);
    if targets.is_empty() {
        return;
    }

    let state = ui.global::<AppState>();
    match field {
        "netease-id" => {
            for &m_idx in &targets {
                c.master_tracks[m_idx].netease_id = value.clone();
                c.other_modified[m_idx] = true;
                c.recompute_track_modified(m_idx);
            }
        }
        "lyrics" => {
            let has_lyrics = !value.trim().is_empty();
            let preview: SharedString = first_lyric_line(&value).into();
            for &m_idx in &targets {
                let t = &mut c.master_tracks[m_idx];
                t.has_lyrics = has_lyrics;
                t.lyrics_preview = preview.clone();
                t.lyrics = value.clone();
                c.field_modes[m_idx][12] = 0;
                c.recompute_track_modified(m_idx);
            }
            state.set_field_lyrics_mode(0);
            state.set_field_lyrics_mixed(false);
        }
        "is-instrumental" => {
            let inst = value == "true" || value == "1";
            for &m_idx in &targets {
                c.master_tracks[m_idx].is_instrumental = inst;
                c.other_modified[m_idx] = true;
                c.recompute_track_modified(m_idx);
            }
        }
        _ => {
            let Some(f_idx) = field_index(field) else {
                return;
            };
            if f_idx >= 11 {
                return;
            }
            for &m_idx in &targets {
                set_track_text_field(&mut c.master_tracks[m_idx], field, value.clone());
                c.field_modes[m_idx][f_idx] = 0;
                c.recompute_track_modified(m_idx);
            }
            set_inspector_field(
                &state,
                field,
                InspectorField {
                    text: value,
                    mode: 0,
                    mixed: false,
                },
            );
        }
    }

    c.sync_visible_tracks();
    c.sync_counts_and_status(ui, Some(&format!("已编辑字段: {field}")));
}

fn apply_field_mode(ui: &AppWindow, c: &mut AppController, field: &str, mode: i32) {
    let Some(f_idx) = field_index(field) else {
        return;
    };
    let targets = c.target_master_indices(ui);
    if targets.is_empty() {
        return;
    }

    let state = ui.global::<AppState>();
    let normalized_mode = if mode == 2 { 2 } else { 1 };

    for &m_idx in &targets {
        if normalized_mode == 2 {
            clear_track_field(&mut c.master_tracks[m_idx], field);
            c.field_modes[m_idx][f_idx] = 2;
        } else {
            let orig = c.original_tracks[m_idx].clone();
            restore_track_field(&mut c.master_tracks[m_idx], &orig, field);
            c.field_modes[m_idx][f_idx] = 1;
        }
        c.recompute_track_modified(m_idx);
    }

    c.sync_visible_tracks();
    c.sync_selected_track_inputs(ui);

    // Ensure explicit single/multi clear badge stays exact even when clearing an already-empty field.
    if normalized_mode == 2 {
        match field {
            "cover" => {
                state.set_field_cover_mode(2);
                state.set_field_cover_mixed(false);
            }
            "lyrics" => {
                state.set_field_lyrics_mode(2);
                state.set_field_lyrics_mixed(false);
            }
            _ => {
                set_inspector_field(
                    &state,
                    field,
                    InspectorField {
                        text: "".into(),
                        mode: 2,
                        mixed: false,
                    },
                );
            }
        }
    }

    let action_label = if normalized_mode == 2 { "置空" } else { "保留" };
    c.sync_counts_and_status(ui, Some(&format!("字段 {field} 已设为{action_label}")));
}

fn init_state(ui: &AppWindow, ctrl: &AppController) {
    ui.global::<Theme>().set_preference(ThemePreference::Dark);

    let state = ui.global::<AppState>();
    state.set_tracks(ModelRc::from(ctrl.tracks_model.clone()));
    state.set_search_candidates(ModelRc::from(ctrl.candidates_model.clone()));
    state.set_cover_candidates(ModelRc::from(ctrl.covers_model.clone()));
    state.set_columns(ModelRc::from(ctrl.columns_model.clone()));
    state.set_dirs(ModelRc::from(ctrl.dirs_model.clone()));
    state.set_history(ModelRc::from(ctrl.history_model.clone()));
    state.set_regex_groups(ModelRc::from(ctrl.regex_groups_model.clone()));
    state.set_filename_previews(ModelRc::from(ctrl.filename_previews_model.clone()));
    state.set_auto_match_rows(ModelRc::from(ctrl.auto_match_model.clone()));
    state.set_charset_rows(ModelRc::from(ctrl.charset_model.clone()));
    state.set_source_items(ModelRc::from(ctrl.sources_model.clone()));
    state.set_selected_source_index(0);
    state.set_selected_index(0);
    state.set_working_dir("~/Music".into());
    state.set_progress_title("就绪".into());
    state.set_progress_file("".into());
    state.set_progress_value(0.0);
    state.set_progress_ok(0);
    state.set_progress_fail(0);
    state.set_progress_skip(0);
    state.set_progress_total(0);

    ctrl.sync_selected_track_inputs(ui);
    ctrl.sync_counts_and_status(ui, Some("~/Music"));
}

fn wire_callbacks(ui: &AppWindow, ctrl: Rc<RefCell<AppController>>) {
    let logic = ui.global::<AppLogic>();

    {
        let ui_w = ui.as_weak();
        let ctrl = ctrl.clone();
        logic.on_open_dialog(move |dialog| {
            let Some(ui) = ui_w.upgrade() else { return };
            let c = ctrl.borrow();
            let state = ui.global::<AppState>();
            let prev_dialog = state.get_active_dialog();
            match dialog {
                ActiveDialog::ComboSearch | ActiveDialog::LyricSearch | ActiveDialog::CoverSearch => {
                    if let Some(m_idx) = c.selected_master_index(&ui) {
                        let track = &c.master_tracks[m_idx];
                        state.set_combo_query_title(track.title.clone());
                        state.set_combo_query_artist(track.artist.clone());
                        state.set_combo_query_album(track.album.clone());
                        state.set_combo_query_id(track.netease_id.clone());
                    }
                }
                ActiveDialog::LyricEditor => {
                    if prev_dialog == ActiveDialog::None {
                        if let Some(m_idx) = c.selected_master_index(&ui) {
                            state.set_lyric_editor_text(c.master_tracks[m_idx].lyrics.clone());
                        }
                    }
                }
                ActiveDialog::Progress => {
                    if state.get_progress_total() == 0 {
                        state.set_progress_title("正在计算 ReplayGain 回放增益...".into());
                        state.set_progress_file("初音ミク,n-buna - ウミユリ海底譚.flac".into());
                        state.set_progress_value(0.625);
                        state.set_progress_ok(5);
                        state.set_progress_fail(0);
                        state.set_progress_skip(1);
                        state.set_progress_total(8);
                    }
                }
                _ => {}
            }
            state.set_active_dialog(dialog);
        });
    }

    {
        let ui_w = ui.as_weak();
        logic.on_close_dialog(move || {
            let Some(ui) = ui_w.upgrade() else { return };
            ui.global::<AppState>().set_active_dialog(ActiveDialog::None);
        });
    }

    {
        let ui_w = ui.as_weak();
        let ctrl = ctrl.clone();
        logic.on_select_track(move |idx| {
            let Some(ui) = ui_w.upgrade() else { return };
            let c = ctrl.borrow();
            if idx >= 0 && (idx as usize) < c.filtered_indices.len() {
                ui.global::<AppState>().set_selected_index(idx);
                c.sync_selected_track_inputs(&ui);
                let track = &c.master_tracks[c.filtered_indices[idx as usize]];
                c.sync_counts_and_status(&ui, Some(&track.filename));
            }
        });
    }

    {
        let ui_w = ui.as_weak();
        let ctrl = ctrl.clone();
        logic.on_toggle_track_selected(move |idx| {
            let Some(ui) = ui_w.upgrade() else { return };
            let mut c = ctrl.borrow_mut();
            if idx >= 0 {
                let v_idx = idx as usize;
                if let Some(&m_idx) = c.filtered_indices.get(v_idx) {
                    c.master_tracks[m_idx].selected = !c.master_tracks[m_idx].selected;
                    let updated = c.master_tracks[m_idx].clone();
                    c.update_visible_track(v_idx, updated);
                    c.sync_selected_track_inputs(&ui);
                    c.sync_counts_and_status(&ui, None);
                }
            }
        });
    }

    {
        let ui_w = ui.as_weak();
        let ctrl = ctrl.clone();
        logic.on_select_all(move || {
            let Some(ui) = ui_w.upgrade() else { return };
            let mut c = ctrl.borrow_mut();
            let indices = c.filtered_indices.clone();
            for (v_idx, m_idx) in indices.into_iter().enumerate() {
                c.master_tracks[m_idx].selected = true;
                let updated = c.master_tracks[m_idx].clone();
                c.update_visible_track(v_idx, updated);
            }
            c.sync_selected_track_inputs(&ui);
            c.sync_counts_and_status(&ui, Some("已全选"));
        });
    }

    {
        let ui_w = ui.as_weak();
        let ctrl = ctrl.clone();
        logic.on_unselect_all(move || {
            let Some(ui) = ui_w.upgrade() else { return };
            let mut c = ctrl.borrow_mut();
            let indices = c.filtered_indices.clone();
            for (v_idx, m_idx) in indices.into_iter().enumerate() {
                c.master_tracks[m_idx].selected = false;
                let updated = c.master_tracks[m_idx].clone();
                c.update_visible_track(v_idx, updated);
            }
            c.sync_selected_track_inputs(&ui);
            c.sync_counts_and_status(&ui, Some("已取消全选"));
        });
    }

    {
        let ui_w = ui.as_weak();
        let ctrl = ctrl.clone();
        logic.on_invert_selection(move || {
            let Some(ui) = ui_w.upgrade() else { return };
            let mut c = ctrl.borrow_mut();
            let indices = c.filtered_indices.clone();
            for (v_idx, m_idx) in indices.into_iter().enumerate() {
                c.master_tracks[m_idx].selected = !c.master_tracks[m_idx].selected;
                let updated = c.master_tracks[m_idx].clone();
                c.update_visible_track(v_idx, updated);
            }
            c.sync_selected_track_inputs(&ui);
            c.sync_counts_and_status(&ui, Some("已反选"));
        });
    }

    {
        let ui_w = ui.as_weak();
        let ctrl = ctrl.clone();
        logic.on_save_selected(move || {
            let Some(ui) = ui_w.upgrade() else { return };
            let mut c = ctrl.borrow_mut();
            let any_selected = c.master_tracks.iter().any(|t| t.selected && t.modified);
            let mut saved_count = 0usize;
            let mut last_saved: Option<TrackItem> = None;

            for i in 0..c.master_tracks.len() {
                if c.master_tracks[i].modified && (!any_selected || c.master_tracks[i].selected) {
                    c.master_tracks[i].modified = false;
                    c.master_tracks[i].update_time = "2026-11-24 02:58:00".into();
                    c.original_tracks[i] = c.master_tracks[i].clone();
                    c.initial_modified[i] = false;
                    c.other_modified[i] = false;
                    c.field_modes[i] = [1; 13];
                    saved_count += 1;
                    last_saved = Some(c.master_tracks[i].clone());
                }
            }
            c.sync_visible_tracks();
            if let Some(t) = last_saved {
                c.history_model.insert(
                    0,
                    HistoryEntry {
                        timestamp: "2026-11-24 02:58:00".into(),
                        summary: format!("保存标签变更 ({saved_count} 首曲目)").into(),
                        title: t.title,
                        artist: t.artist,
                        album: t.album,
                        comment_has_163: t.has_163_key,
                    },
                );
            }
            c.sync_selected_track_inputs(&ui);
            c.sync_counts_and_status(&ui, Some(&format!("已保存 {saved_count} 首修改")));
        });
    }

    {
        let ui_w = ui.as_weak();
        let ctrl = ctrl.clone();
        logic.on_revert_selected(move || {
            let Some(ui) = ui_w.upgrade() else { return };
            let mut c = ctrl.borrow_mut();
            let cur_m = c.selected_master_index(&ui);
            let any_selected = c.master_tracks.iter().any(|t| t.selected);
            let mut reverted = 0usize;

            for i in 0..c.master_tracks.len() {
                let target = if any_selected {
                    c.master_tracks[i].selected
                } else {
                    Some(i) == cur_m
                };
                if target {
                    let keep_sel = c.master_tracks[i].selected;
                    c.master_tracks[i] = c.original_tracks[i].clone();
                    c.master_tracks[i].selected = keep_sel;
                    c.master_tracks[i].modified = false;
                    c.initial_modified[i] = false;
                    c.other_modified[i] = false;
                    c.field_modes[i] = [1; 13];
                    reverted += 1;
                }
            }
            c.sync_visible_tracks();
            c.sync_selected_track_inputs(&ui);
            c.sync_counts_and_status(&ui, Some(&format!("已撤销 {reverted} 首未保存变更")));
        });
    }

    {
        let ui_w = ui.as_weak();
        let ctrl = ctrl.clone();
        logic.on_set_theme(move |pref| {
            let Some(ui) = ui_w.upgrade() else { return };
            let (variant, label) = match pref {
                0 => (ThemePreference::System, "跟随系统主题"),
                1 => (ThemePreference::Light, "浅色主题"),
                _ => (ThemePreference::Dark, "深色主题"),
            };
            ui.global::<Theme>().set_preference(variant);
            ctrl.borrow().sync_counts_and_status(&ui, Some(label));
        });
    }

    {
        let ui_w = ui.as_weak();
        let ctrl = ctrl.clone();
        logic.on_update_filter(move |field_idx, keyword| {
            let Some(ui) = ui_w.upgrade() else { return };
            ctrl.borrow_mut()
                .rebuild_filtered_tracks(&ui, field_idx, &keyword);
        });
    }

    {
        let ui_w = ui.as_weak();
        let ctrl = ctrl.clone();
        logic.on_edit_selected_field(move |field, value| {
            let Some(ui) = ui_w.upgrade() else { return };
            let mut c = ctrl.borrow_mut();
            apply_edit_field(&ui, &mut c, field.as_str(), value);
        });
    }

    {
        let ui_w = ui.as_weak();
        let ctrl = ctrl.clone();
        logic.on_set_field_mode(move |field, mode| {
            let Some(ui) = ui_w.upgrade() else { return };
            let mut c = ctrl.borrow_mut();
            apply_field_mode(&ui, &mut c, field.as_str(), mode);
        });
    }

    {
        let ui_w = ui.as_weak();
        let ctrl = ctrl.clone();
        logic.on_apply_combo_candidate(move |cand_idx, apply_all| {
            let Some(ui) = ui_w.upgrade() else { return };
            let mut c = ctrl.borrow_mut();
            let state = ui.global::<AppState>();
            let Some(cand) = c.candidates_model.row_data(cand_idx as usize) else {
                return;
            };
            let v_idx = state.get_selected_index();
            if v_idx < 0 {
                return;
            }
            let v_idx = v_idx as usize;
            let Some(&m_idx) = c.filtered_indices.get(v_idx) else {
                return;
            };
            if apply_all || state.get_combo_overwrite_title() {
                c.master_tracks[m_idx].title = cand.title.clone();
                c.field_modes[m_idx][0] = 0;
            }
            if apply_all || state.get_combo_overwrite_artist() {
                c.master_tracks[m_idx].artist = cand.artist.clone();
                c.field_modes[m_idx][1] = 0;
            }
            if apply_all || state.get_combo_overwrite_album() {
                c.master_tracks[m_idx].album = cand.album.clone();
                c.field_modes[m_idx][2] = 0;
            }
            if apply_all || state.get_combo_overwrite_year() {
                c.master_tracks[m_idx].year = cand.year.clone();
                c.field_modes[m_idx][4] = 0;
            }
            if apply_all || state.get_combo_overwrite_comment() {
                if state.get_opt_write_163_key() && cand.source == "网易云音乐" {
                    c.master_tracks[m_idx].has_163_key = true;
                    c.master_tracks[m_idx].netease_id = cand.song_id.clone();
                    c.master_tracks[m_idx].comment =
                        format!("163 key(Don't modify):id={}", cand.song_id).into();
                    c.field_modes[m_idx][10] = 0;
                }
            }
            if apply_all || state.get_combo_overwrite_cover() {
                c.master_tracks[m_idx].cover = cand.cover.clone();
                c.master_tracks[m_idx].has_picture = true;
                c.master_tracks[m_idx].cover_info =
                    format!("{} · JPEG · Front Cover", cand.cover_size).into();
                c.field_modes[m_idx][11] = 0;
            }
            if (apply_all || state.get_combo_overwrite_lyrics())
                && !(state.get_opt_skip_instrumental() && cand.is_instrumental)
            {
                c.master_tracks[m_idx].lyrics = cand.lyric_preview.clone();
                c.master_tracks[m_idx].lyrics_preview =
                    first_lyric_line(&cand.lyric_preview).into();
                c.master_tracks[m_idx].has_lyrics = !cand.lyric_preview.trim().is_empty();
                c.master_tracks[m_idx].is_instrumental = cand.is_instrumental;
                c.field_modes[m_idx][12] = 0;
            }
            c.other_modified[m_idx] = true;
            c.recompute_track_modified(m_idx);
            let updated = c.master_tracks[m_idx].clone();
            c.update_visible_track(v_idx, updated);
            state.set_active_dialog(ActiveDialog::None);
            c.sync_selected_track_inputs(&ui);
            c.sync_counts_and_status(&ui, Some(&format!("已应用组合标签: {}", cand.title)));
        });
    }

    {
        let ui_w = ui.as_weak();
        let ctrl = ctrl.clone();
        logic.on_apply_lyric_candidate(move |cand_idx| {
            let Some(ui) = ui_w.upgrade() else { return };
            let mut c = ctrl.borrow_mut();
            let state = ui.global::<AppState>();
            let Some(cand) = c.candidates_model.row_data(cand_idx as usize) else {
                return;
            };
            let v_idx = state.get_selected_index();
            if v_idx < 0 {
                return;
            }
            let v_idx = v_idx as usize;
            let Some(&m_idx) = c.filtered_indices.get(v_idx) else {
                return;
            };
            {
                let t = &mut c.master_tracks[m_idx];
                t.lyrics = cand.lyric_preview.clone();
                t.lyrics_preview = first_lyric_line(&cand.lyric_preview).into();
                t.has_lyrics = !cand.lyric_preview.trim().is_empty();
                t.is_instrumental = cand.is_instrumental;
            }
            c.field_modes[m_idx][12] = 0;
            c.recompute_track_modified(m_idx);
            let updated = c.master_tracks[m_idx].clone();
            c.update_visible_track(v_idx, updated);
            state.set_active_dialog(ActiveDialog::None);
            c.sync_selected_track_inputs(&ui);
            c.sync_counts_and_status(&ui, Some(&format!("已下载歌词 ({})", cand.source)));
        });
    }

    {
        let ui_w = ui.as_weak();
        let ctrl = ctrl.clone();
        logic.on_apply_cover_candidate(move |cov_idx| {
            let Some(ui) = ui_w.upgrade() else { return };
            let mut c = ctrl.borrow_mut();
            let state = ui.global::<AppState>();
            let Some(cov) = c.covers_model.row_data(cov_idx as usize) else {
                return;
            };
            let v_idx = state.get_selected_index();
            if v_idx < 0 {
                return;
            }
            let v_idx = v_idx as usize;
            let Some(&m_idx) = c.filtered_indices.get(v_idx) else {
                return;
            };
            {
                let t = &mut c.master_tracks[m_idx];
                t.cover = cov.cover.clone();
                t.has_picture = true;
                t.cover_info =
                    format!("{} · {} · {}", cov.resolution, cov.format, cov.size_kb).into();
            }
            c.field_modes[m_idx][11] = 0;
            c.recompute_track_modified(m_idx);
            let updated = c.master_tracks[m_idx].clone();
            c.update_visible_track(v_idx, updated);
            state.set_active_dialog(ActiveDialog::None);
            c.sync_selected_track_inputs(&ui);
            c.sync_counts_and_status(&ui, Some(&format!("已应用封面 ({})", cov.source)));
        });
    }

    {
        let ui_w = ui.as_weak();
        let ctrl = ctrl.clone();
        logic.on_lyric_action(move |action| {
            let Some(ui) = ui_w.upgrade() else { return };
            let state = ui.global::<AppState>();
            let text = state.get_lyric_editor_text().to_string();
            let mut c = ctrl.borrow_mut();

            match action.as_str() {
                "delete-head-tag" => {
                    let cleaned = delete_head_tags(&text);
                    state.set_lyric_editor_text(cleaned.into());
                    c.sync_counts_and_status(&ui, Some("已删除歌词头部标签"));
                }
                "delete-blank-lines" => {
                    let cleaned = delete_blank_lyric_lines(&text);
                    state.set_lyric_editor_text(cleaned.into());
                    c.sync_counts_and_status(&ui, Some("已删除空白文字行"));
                }
                "reformat-timetag" => {
                    let cleaned = reformat_timetags(&text);
                    state.set_lyric_editor_text(cleaned.into());
                    c.sync_counts_and_status(&ui, Some("已格式化时间轴 (2位毫秒)"));
                }
                "remove-timetag" => {
                    let cleaned = remove_timetags(&text);
                    state.set_lyric_editor_text(cleaned.into());
                    c.sync_counts_and_status(&ui, Some("已移除所有时间轴"));
                }
                "shift-timetag" => {
                    let offset: i64 = state.get_adjust_offset_ms().trim().parse().unwrap_or(0);
                    let shifted = shift_timetags(&text, offset);
                    state.set_lyric_editor_text(shifted.into());
                    state.set_active_dialog(ActiveDialog::LyricEditor);
                    c.sync_counts_and_status(&ui, Some(&format!("时间轴已平移 {offset} ms")));
                }
                "reset" => {
                    c.sync_selected_track_inputs(&ui);
                    c.sync_counts_and_status(&ui, Some("已重置歌词编辑器内容"));
                }
                "apply" => {
                    let v_idx = state.get_selected_index();
                    if v_idx >= 0 {
                        let v_idx = v_idx as usize;
                        if let Some(&m_idx) = c.filtered_indices.get(v_idx) {
                            let new_lyric = state.get_lyric_editor_text();
                            {
                                let t = &mut c.master_tracks[m_idx];
                                t.has_lyrics = !new_lyric.trim().is_empty();
                                t.lyrics_preview = first_lyric_line(&new_lyric).into();
                                t.lyrics = new_lyric;
                            }
                            c.field_modes[m_idx][12] = 0;
                            c.recompute_track_modified(m_idx);
                            let updated = c.master_tracks[m_idx].clone();
                            c.update_visible_track(v_idx, updated);
                        }
                    }
                    state.set_active_dialog(ActiveDialog::None);
                    c.sync_selected_track_inputs(&ui);
                    c.sync_counts_and_status(&ui, Some("已应用歌词修改"));
                }
                "import-lrc" => {
                    c.sync_counts_and_status(&ui, Some("已从同名 .lrc 文件载入歌词"));
                }
                "save-lrc" => {
                    c.sync_counts_and_status(&ui, Some("已导出为同名 UTF-8 .lrc 文件"));
                }
                _ => {}
            }
        });
    }

    {
        let ui_w = ui.as_weak();
        let ctrl = ctrl.clone();
        logic.on_move_column(move |idx, delta| {
            let Some(ui) = ui_w.upgrade() else { return };
            let c = ctrl.borrow();
            let count = c.columns_model.row_count() as i32;
            let target = idx + delta;
            if idx >= 0 && target >= 0 && idx < count && target < count {
                let a = c.columns_model.row_data(idx as usize).unwrap();
                let b = c.columns_model.row_data(target as usize).unwrap();
                c.columns_model.set_row_data(idx as usize, b);
                c.columns_model.set_row_data(target as usize, a);
                ui.global::<AppState>().set_selected_column_index(target);
            }
        });
    }

    {
        let ui_w = ui.as_weak();
        let ctrl = ctrl.clone();
        logic.on_reset_columns(move || {
            let Some(ui) = ui_w.upgrade() else { return };
            let c = ctrl.borrow();
            c.columns_model.set_vec(sample_data::default_columns());
            ui.global::<AppState>().set_selected_column_index(0);
            c.sync_counts_and_status(&ui, Some("显示列已重置为默认"));
        });
    }

    {
        let ui_w = ui.as_weak();
        let ctrl = ctrl.clone();
        logic.on_toggle_column(move |idx| {
            let Some(ui) = ui_w.upgrade() else { return };
            let c = ctrl.borrow();
            if idx >= 0 && (idx as usize) < c.columns_model.row_count() {
                if let Some(mut col) = c.columns_model.row_data(idx as usize) {
                    col.visible = !col.visible;
                    c.columns_model.set_row_data(idx as usize, col);
                    ui.global::<AppState>().set_selected_column_index(idx);
                }
            }
        });
    }

    {
        let ui_w = ui.as_weak();
        let ctrl = ctrl.clone();
        logic.on_toggle_source(move |idx| {
            let Some(ui) = ui_w.upgrade() else { return };
            let c = ctrl.borrow();
            if idx >= 0 && (idx as usize) < c.sources_model.row_count() {
                if let Some(mut item) = c.sources_model.row_data(idx as usize) {
                    item.enabled = !item.enabled;
                    c.sources_model.set_row_data(idx as usize, item);
                    ui.global::<AppState>().set_selected_source_index(idx);
                }
            }
        });
    }

    {
        let ui_w = ui.as_weak();
        let ctrl = ctrl.clone();
        logic.on_move_source(move |idx, delta| {
            let Some(ui) = ui_w.upgrade() else { return };
            let c = ctrl.borrow();
            let count = c.sources_model.row_count() as i32;
            let target = idx + delta;
            if idx >= 0 && target >= 0 && idx < count && target < count {
                let a = c.sources_model.row_data(idx as usize).unwrap();
                let b = c.sources_model.row_data(target as usize).unwrap();
                c.sources_model.set_row_data(idx as usize, b);
                c.sources_model.set_row_data(target as usize, a);
                ui.global::<AppState>().set_selected_source_index(target);
            }
        });
    }

    {
        let ui_w = ui.as_weak();
        let ctrl = ctrl.clone();
        logic.on_reload_plugins(move || {
            let Some(ui) = ui_w.upgrade() else { return };
            let c = ctrl.borrow();
            c.sources_model.set_vec(sample_source_items());
            ui.global::<AppState>().set_selected_source_index(0);
        });
    }

    {
        let ui_w = ui.as_weak();
        let ctrl = ctrl.clone();
        logic.on_trigger_action(move |action| {
            let Some(ui) = ui_w.upgrade() else { return };
            let mut c = ctrl.borrow_mut();
            let state = ui.global::<AppState>();

            match action.as_str() {
                "refresh" => {
                    c.rebuild_filtered_tracks(&ui, 0, "");
                    c.sync_counts_and_status(&ui, Some("已刷新曲目列表"));
                }
                "delete-cover" => {
                    apply_field_mode(&ui, &mut c, "cover", 2);
                }
                "extract-cover" => {
                    c.sync_counts_and_status(&ui, Some("已提取封面到歌曲所在目录"));
                }
                "clear-tags" => {
                    let any_selected = c.master_tracks.iter().any(|t| t.selected);
                    let cur_m = c.selected_master_index(&ui);
                    for i in 0..c.master_tracks.len() {
                        if (any_selected && c.master_tracks[i].selected)
                            || (!any_selected && Some(i) == cur_m)
                        {
                            clear_track_field(&mut c.master_tracks[i], "album");
                            clear_track_field(&mut c.master_tracks[i], "comment");
                            c.field_modes[i][2] = 2;
                            c.field_modes[i][10] = 2;
                            c.recompute_track_modified(i);
                        }
                    }
                    c.sync_visible_tracks();
                    c.sync_selected_track_inputs(&ui);
                    c.sync_counts_and_status(&ui, Some("已清除所选标签字段"));
                }
                "replaygain" => {
                    state.set_progress_title("正在扫描并写入 ReplayGain 回放增益...".into());
                    state.set_progress_file("3R2 - New Quest.flac".into());
                    state.set_progress_value(1.0);
                    state.set_progress_ok(7);
                    state.set_progress_fail(0);
                    state.set_progress_skip(1);
                    state.set_progress_total(8);
                    state.set_active_dialog(ActiveDialog::Progress);
                    c.sync_counts_and_status(&ui, Some("ReplayGain 计算完成"));
                }
                "auto-match" => {
                    state.set_active_dialog(ActiveDialog::AutoMatch);
                    c.sync_counts_and_status(&ui, Some("自动匹配标签已完成预览"));
                }
                "find-replace" => {
                    let find = state.get_find_text().to_string();
                    let replace = state.get_replace_text().to_string();
                    if !find.is_empty() {
                        let match_case = state.get_find_match_case();
                        let mut changed = 0usize;
                        for i in 0..c.master_tracks.len() {
                            let new_title =
                                replace_str(&c.master_tracks[i].title, &find, &replace, match_case);
                            let new_album =
                                replace_str(&c.master_tracks[i].album, &find, &replace, match_case);
                            let title_diff = new_title != c.master_tracks[i].title.as_str();
                            let album_diff = new_album != c.master_tracks[i].album.as_str();
                            if title_diff || album_diff {
                                if title_diff {
                                    c.master_tracks[i].title = new_title.into();
                                    c.field_modes[i][0] = 0;
                                }
                                if album_diff {
                                    c.master_tracks[i].album = new_album.into();
                                    c.field_modes[i][2] = 0;
                                }
                                c.recompute_track_modified(i);
                                changed += 1;
                            }
                        }
                        c.sync_visible_tracks();
                        c.sync_selected_track_inputs(&ui);
                        c.sync_counts_and_status(&ui, Some(&format!("查找替换完成 ({changed} 首)")));
                    }
                    state.set_active_dialog(ActiveDialog::None);
                }
                "apply-cover-resolution" => {
                    if let Some(m_idx) = c.selected_master_index(&ui) {
                        let v_idx = state.get_selected_index() as usize;
                        let w = state.get_cover_target_width();
                        let h = state.get_cover_target_height();
                        let kb = state.get_cover_target_max_kb();
                        let fmt = if state.get_cover_format_index() == 2 {
                            "PNG"
                        } else {
                            "JPEG"
                        };
                        if c.master_tracks[m_idx].has_picture {
                            c.master_tracks[m_idx].cover_info =
                                format!("{w}×{h} · {fmt} · ≤{kb} KB").into();
                            c.field_modes[m_idx][11] = 0;
                            c.recompute_track_modified(m_idx);
                            let updated = c.master_tracks[m_idx].clone();
                            c.update_visible_track(v_idx, updated);
                        }
                    }
                    state.set_active_dialog(ActiveDialog::None);
                    c.sync_selected_track_inputs(&ui);
                    c.sync_counts_and_status(&ui, Some("已调整封面分辨率"));
                }
                "apply-filename" => {
                    state.set_active_dialog(ActiveDialog::None);
                    c.sync_counts_and_status(&ui, Some("已执行文件名/标签批量映射"));
                }
                "apply-charset" => {
                    state.set_active_dialog(ActiveDialog::None);
                    c.sync_counts_and_status(&ui, Some("已修正所选曲目字符编码"));
                }
                "restore-history" => {
                    let h_idx = state.get_selected_history_index();
                    if h_idx >= 0 {
                        if let Some(entry) = c.history_model.row_data(h_idx as usize) {
                            if let Some(m_idx) = c.selected_master_index(&ui) {
                                let v_idx = state.get_selected_index() as usize;
                                {
                                    let t = &mut c.master_tracks[m_idx];
                                    t.title = entry.title.clone();
                                    t.artist = entry.artist.clone();
                                    t.album = entry.album.clone();
                                }
                                c.field_modes[m_idx][0] = 0;
                                c.field_modes[m_idx][1] = 0;
                                c.field_modes[m_idx][2] = 0;
                                c.recompute_track_modified(m_idx);
                                let updated = c.master_tracks[m_idx].clone();
                                c.update_visible_track(v_idx, updated);
                            }
                        }
                    }
                    state.set_active_dialog(ActiveDialog::None);
                    c.sync_selected_track_inputs(&ui);
                    c.sync_counts_and_status(&ui, Some("已还原所选历史标签快照"));
                }
                "clear-history" => {
                    c.history_model.set_vec(Vec::new());
                    c.sync_counts_and_status(&ui, Some("已清除标签历史记录"));
                }
                "add-dir" => {
                    c.dirs_model.push(DirEntry {
                        path: "~/Music/NewFolder".into(),
                        include_subdirs: true,
                        file_count: 0,
                    });
                    c.sync_counts_and_status(&ui, Some("已添加监听目录"));
                }
                "remove-dir" => {
                    let d_idx = state.get_selected_dir_index();
                    if d_idx >= 0 && (d_idx as usize) < c.dirs_model.row_count() {
                        c.dirs_model.remove(d_idx as usize);
                        state.set_selected_dir_index(0);
                    }
                }
                "toggle-dir-subdirs" => {
                    let d_idx = state.get_selected_dir_index();
                    if d_idx >= 0 && (d_idx as usize) < c.dirs_model.row_count() {
                        if let Some(mut dir) = c.dirs_model.row_data(d_idx as usize) {
                            dir.include_subdirs = !dir.include_subdirs;
                            c.dirs_model.set_row_data(d_idx as usize, dir);
                        }
                    }
                }
                "apply-options" => {
                    state.set_active_dialog(ActiveDialog::None);
                    c.sync_counts_and_status(&ui, Some("选项设置已保存"));
                }
                "reset-options" => {
                    state.set_opt_write_163_key(true);
                    state.set_opt_artist_sep(";".into());
                    state.set_opt_id3v2_ver(1);
                    state.set_opt_dl_trans(true);
                    state.set_opt_skip_instrumental(true);
                    state.set_opt_rg_target_lufs("-18.0".into());
                    c.sync_counts_and_status(&ui, Some("选项已还原为默认值"));
                }
                "search-combo" | "search-lyric" => {
                    c.sync_counts_and_status(&ui, Some("已刷新云端匹配候选结果"));
                }
                "search-cover" => {
                    c.sync_counts_and_status(&ui, Some("已刷新封面候选列表"));
                }
                _ => {
                    c.sync_counts_and_status(&ui, Some(&format!("已触发操作: {action}")));
                }
            }
        });
    }
}

fn first_lyric_line(lyrics: &str) -> String {
    lyrics
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

fn delete_head_tags(lyrics: &str) -> String {
    lyrics
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            !(trimmed.starts_with("[ti:")
                || trimmed.starts_with("[ar:")
                || trimmed.starts_with("[al:")
                || trimmed.starts_with("[by:")
                || trimmed.starts_with("[offset:"))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn delete_blank_lyric_lines(lyrics: &str) -> String {
    lyrics
        .lines()
        .filter(|line| {
            let stripped = remove_timetags(line);
            !stripped.trim().is_empty()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn reformat_timetags(lyrics: &str) -> String {
    lyrics
        .lines()
        .map(|line| {
            let mut out = String::with_capacity(line.len());
            let bytes = line.as_bytes();
            let mut i = 0;
            while i < bytes.len() {
                if bytes[i] == b'['
                    && i + 10 < bytes.len()
                    && bytes[i + 3] == b':'
                    && bytes[i + 6] == b'.'
                    && bytes[i + 10] == b']'
                {
                    out.push_str(&line[i..i + 9]);
                    out.push(']');
                    i += 11;
                } else if let Some(ch) = line[i..].chars().next() {
                    out.push(ch);
                    i += ch.len_utf8();
                } else {
                    break;
                }
            }
            out
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn remove_timetags(lyrics: &str) -> String {
    let mut out = String::with_capacity(lyrics.len());
    let mut chars = lyrics.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '[' {
            let mut buf = String::new();
            let mut closed = false;
            for next_c in chars.by_ref() {
                if next_c == ']' {
                    closed = true;
                    break;
                }
                buf.push(next_c);
            }
            let looks_like_time = closed
                && buf.contains(':')
                && buf
                    .chars()
                    .all(|ch| ch.is_ascii_digit() || ch == ':' || ch == '.');
            if !looks_like_time {
                out.push('[');
                out.push_str(&buf);
                if closed {
                    out.push(']');
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn shift_timetags(lyrics: &str, offset_ms: i64) -> String {
    lyrics
        .lines()
        .map(|line| {
            if let Some(end) = line.find(']') {
                if line.starts_with('[') {
                    let tag = &line[1..end];
                    if let Some((min_s, sec_s)) = tag.split_once(':') {
                        if let (Ok(mins), Ok(secs)) = (min_s.parse::<i64>(), sec_s.parse::<f64>())
                        {
                            let total_ms =
                                (mins * 60_000 + (secs * 1000.0).round() as i64 + offset_ms)
                                    .max(0);
                            let new_m = total_ms / 60_000;
                            let new_s = (total_ms % 60_000) / 1000;
                            let new_cs = (total_ms % 1000) / 10;
                            return format!(
                                "[{new_m:02}:{new_s:02}.{new_cs:02}]{}",
                                &line[end + 1..]
                            );
                        }
                    }
                }
            }
            line.to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn replace_str(input: &str, find: &str, replace: &str, match_case: bool) -> String {
    if match_case || find.is_empty() {
        input.replace(find, replace)
    } else {
        let lower_in = input.to_lowercase();
        let lower_find = find.to_lowercase();
        if lower_in.len() == input.len() && lower_find.len() == find.len() {
            let mut out = String::new();
            let mut idx = 0;
            while let Some(pos) = lower_in[idx..].find(&lower_find) {
                let start = idx + pos;
                let end = start + find.len();
                if input.is_char_boundary(start) && input.is_char_boundary(end) {
                    out.push_str(&input[idx..start]);
                    out.push_str(replace);
                    idx = end;
                } else {
                    break;
                }
            }
            out.push_str(&input[idx..]);
            out
        } else {
            input.replace(find, replace)
        }
    }
}

fn write_snapshot_png(
    ui: &AppWindow,
    clock: &Rc<Cell<Duration>>,
    out_dir: &Path,
    filename: &str,
) -> Result<(), Box<dyn Error>> {
    // Flush layout and advance monotonic animation clock so transitions settle completely.
    slint::platform::update_timers_and_animations();
    clock.set(clock.get() + Duration::from_millis(500));
    slint::platform::update_timers_and_animations();

    let snapshot = ui.window().take_snapshot()?;
    let out_path = out_dir.join(filename);
    let file = std::fs::File::create(&out_path)?;
    let mut encoder = png::Encoder::new(file, snapshot.width(), snapshot.height());
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(snapshot.as_bytes())?;
    println!("Rendered {}", out_path.display());
    Ok(())
}

fn render_baseline(out_dir: &Path) -> Result<(), Box<dyn Error>> {
    std::fs::create_dir_all(out_dir)?;
    let clock = Rc::new(Cell::new(Duration::from_millis(10)));
    let msw = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
    slint::platform::set_platform(Box::new(HeadlessPlatform {
        window: msw.clone(),
        clock: clock.clone(),
    }))?;

    let ui = AppWindow::new()?;
    let ctrl = Rc::new(RefCell::new(AppController::new()));
    init_state(&ui, &ctrl.borrow());
    wire_callbacks(&ui, ctrl.clone());

    msw.set_size(PhysicalSize::new(1440, 900));
    ui.show()?;

    let set_view = |theme: ThemePreference, dialog: ActiveDialog, opt_tab: i32, fn_tab: i32| {
        ui.global::<Theme>().set_preference(theme);
        let state = ui.global::<AppState>();
        state.set_options_tab_index(opt_tab);
        state.set_filename_rel_tab_index(fn_tab);
        state.set_active_dialog(dialog);
    };

    // 1. Single-track default main renders
    set_view(ThemePreference::Dark, ActiveDialog::None, 0, 0);
    write_snapshot_png(&ui, &clock, out_dir, "main_dark.png")?;
    set_view(ThemePreference::Light, ActiveDialog::None, 0, 0);
    write_snapshot_png(&ui, &clock, out_dir, "main_light.png")?;

    // 2. Multi-select tri-state renders (check tracks 0 & 1, simulate mixed + clear + edited states)
    {
        let mut c = ctrl.borrow_mut();
        c.master_tracks[1].selected = true;
        c.sync_visible_tracks();
        c.sync_selected_track_inputs(&ui);
        apply_field_mode(&ui, &mut c, "genre", 2);
        apply_edit_field(&ui, &mut c, "album-artist", "ヨルシカ / n-buna".into());
        apply_field_mode(&ui, &mut c, "lyrics", 2);
    }
    set_view(ThemePreference::Dark, ActiveDialog::None, 0, 0);
    write_snapshot_png(&ui, &clock, out_dir, "main_multiselect_dark.png")?;
    set_view(ThemePreference::Light, ActiveDialog::None, 0, 0);
    write_snapshot_png(&ui, &clock, out_dir, "main_multiselect_light.png")?;

    // Revert back to single-track state before rendering the remaining dialogs
    {
        let mut c = ctrl.borrow_mut();
        for idx in [0usize, 1usize] {
            c.master_tracks[idx] = c.original_tracks[idx].clone();
            c.master_tracks[idx].selected = idx == 0;
            c.field_modes[idx] = [1; 13];
            c.other_modified[idx] = false;
            c.recompute_track_modified(idx);
        }
        c.sync_visible_tracks();
        c.sync_selected_track_inputs(&ui);
        c.sync_counts_and_status(&ui, Some("~/Music"));
    }

    // Prepare rich ReplayGain progress state for progress_rg_dark.png
    let state = ui.global::<AppState>();
    state.set_progress_title("正在计算 ReplayGain 回放增益...".into());
    state.set_progress_file("初音ミク,n-buna - ウミユリ海底譚.flac".into());
    state.set_progress_value(0.625);
    state.set_progress_ok(5);
    state.set_progress_fail(0);
    state.set_progress_skip(1);
    state.set_progress_total(8);
    state.set_find_text("Music".into());
    state.set_replace_text("Audio".into());

    let shots: &[(&str, ThemePreference, ActiveDialog, i32, i32)] = &[
        ("combo_search_dark.png", ThemePreference::Dark, ActiveDialog::ComboSearch, 0, 0),
        ("combo_search_light.png", ThemePreference::Light, ActiveDialog::ComboSearch, 0, 0),
        ("lyric_search_dark.png", ThemePreference::Dark, ActiveDialog::LyricSearch, 0, 0),
        ("lyric_editor_dark.png", ThemePreference::Dark, ActiveDialog::LyricEditor, 0, 0),
        ("cover_search_dark.png", ThemePreference::Dark, ActiveDialog::CoverSearch, 0, 0),
        ("cover_resolution_dark.png", ThemePreference::Dark, ActiveDialog::CoverResolution, 0, 0),
        ("filename_rel_dark.png", ThemePreference::Dark, ActiveDialog::FilenameRel, 0, 0),
        ("filename_rel_regex_dark.png", ThemePreference::Dark, ActiveDialog::FilenameRel, 0, 1),
        ("options_sources_dark.png", ThemePreference::Dark, ActiveDialog::Options, 0, 0),
        ("options_lyrics_dark.png", ThemePreference::Dark, ActiveDialog::Options, 1, 0),
        ("options_misc1_dark.png", ThemePreference::Dark, ActiveDialog::Options, 2, 0),
        ("options_misc2_theme_light.png", ThemePreference::Light, ActiveDialog::Options, 3, 0),
        ("options_replaygain_dark.png", ThemePreference::Dark, ActiveDialog::Options, 4, 0),
        ("auto_match_dark.png", ThemePreference::Dark, ActiveDialog::AutoMatch, 0, 0),
        ("customize_columns_dark.png", ThemePreference::Dark, ActiveDialog::CustomizeColumns, 0, 0),
        ("manage_dirs_dark.png", ThemePreference::Dark, ActiveDialog::ManageDirs, 0, 0),
        ("charset_fix_dark.png", ThemePreference::Dark, ActiveDialog::CharsetFix, 0, 0),
        ("tags_history_dark.png", ThemePreference::Dark, ActiveDialog::TagsHistory, 0, 0),
        ("adjust_timetag_dark.png", ThemePreference::Dark, ActiveDialog::AdjustTimetag, 0, 0),
        ("find_replace_dark.png", ThemePreference::Dark, ActiveDialog::FindReplace, 0, 0),
        ("progress_rg_dark.png", ThemePreference::Dark, ActiveDialog::Progress, 0, 0),
        ("about_dark.png", ThemePreference::Dark, ActiveDialog::About, 0, 0),
    ];

    for &(file, theme, dialog, opt_tab, fn_tab) in shots {
        set_view(theme, dialog, opt_tab, fn_tab);
        write_snapshot_png(&ui, &clock, out_dir, file)?;
    }

    Ok(())
}

fn parse_render_baseline_arg() -> Option<PathBuf> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--render-baseline" {
            return args.next().map(PathBuf::from);
        }
        if let Some(dir) = arg.strip_prefix("--render-baseline=") {
            return Some(PathBuf::from(dir));
        }
    }
    None
}

fn main() -> Result<(), Box<dyn Error>> {
    if let Some(out_dir) = parse_render_baseline_arg() {
        return render_baseline(&out_dir);
    }

    let cli_paths: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    let ui = AppWindow::new()?;
    backend_controller::launch(&ui, cli_paths)?;
    ui.run()?;
    Ok(())
}
