use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::error::Error;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use chrono::Local;
use encoding_rs::GBK;
use image::{DynamicImage, GenericImageView, ImageFormat, ImageReader};
use rayon::prelude::*;
use slint::{ComponentHandle, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString, VecModel};
use walkdir::WalkDir;

use musictag::config::{
    AppConfig, ColumnConfigEntry, DirConfigEntry, HistoryStore, TagHistoryRecord,
};
use musictag::core::batch::{
    RenameStatus, TemplateField, charset_label_from_index, convert_text_zh,
    convert_track_charset_patch, convert_track_zh_patch, execute_batch_renames,
    extract_filename_stem, filename_to_tags, filename_to_tags_regex, plan_batch_renames,
    preview_track_charset, resolve_template, sanitize_filename,
};
use musictag::core::key163::{KEY_163_PREFIX, encode_163_key, extract_163_song_id};
use musictag::core::lyrics::{
    delete_blank_lines, delete_head_tags, process_netease_lyrics, reformat_timetags,
    remove_timetags, shift_timetags,
};
use musictag::core::models::{
    AudioTrack, CandidateTrack, CoverData, CoverSearchItem, CoverWriteSpec, FieldUpdate,
    LyricProcessOptions, PicFormatLimit, ProcessedLyrics, ReplayGainData, RgScanMode, SearchQuery,
    SourceInfo, TagPatch, WriteOptionsConfig, ZhConvMode,
};
use musictag::core::traits::{FormatRegistry, ReplayGainScanner, SourceRegistry, TagSource};
use musictag::formats::{build_default_format_registry, extract_cover_to_sidecar, process_cover_image};
use musictag::replaygain::EbuR128Scanner;
use musictag::sources::build_default_source_registry;

use crate::{
    ActiveDialog, AppLogic, AppState, AppWindow, AutoMatchRow, CharsetPreviewRow, ColumnConfigItem,
    CoverCandidate, DirEntry, FilenamePreviewItem, HistoryEntry, InspectorField, RegexGroupItem,
    SearchCandidate, SourceItem, Theme, ThemePreference, TrackItem,
};

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

const THUMB_MAX_EDGE: u32 = 256;

thread_local! {
    static CONTROLLERS: RefCell<HashMap<u64, Weak<RefCell<BackendController>>>> =
        RefCell::new(HashMap::new());
    static NEXT_INSTANCE_ID: RefCell<u64> = const { RefCell::new(1) };
}

fn alloc_instance_id() -> u64 {
    NEXT_INSTANCE_ID.with(|cell| {
        let mut id = cell.borrow_mut();
        let current = *id;
        *id = current.saturating_add(1);
        current
    })
}

fn register_controller(id: u64, ctrl: &Rc<RefCell<BackendController>>) {
    CONTROLLERS.with(|map| {
        map.borrow_mut().insert(id, Rc::downgrade(ctrl));
    });
}

fn with_controller(id: u64, f: impl FnOnce(&mut BackendController)) {
    let rc_opt = CONTROLLERS.with(|map| {
        let mut guard = map.borrow_mut();
        match guard.get(&id).and_then(Weak::upgrade) {
            Some(rc) => Some(rc),
            None => {
                guard.remove(&id);
                None
            }
        }
    });
    if let Some(rc) = rc_opt {
        if let Ok(mut ctrl) = rc.try_borrow_mut() {
            f(&mut ctrl);
        }
    }
}

struct DecodedRgba {
    rgba: Vec<u8>,
    width: u32,
    height: u32,
}

#[derive(Clone)]
struct ComboOverwriteFlags {
    title: bool,
    artist: bool,
    album: bool,
    album_artist: bool,
    year: bool,
    track: bool,
    disc: bool,
    genre: bool,
    comment: bool,
    lyrics: bool,
    cover: bool,
    write_163_key: bool,
}

impl ComboOverwriteFlags {
    fn from_state(state: &AppState<'_>) -> Self {
        Self {
            title: state.get_combo_overwrite_title(),
            artist: state.get_combo_overwrite_artist(),
            album: state.get_combo_overwrite_album(),
            album_artist: state.get_combo_overwrite_album_artist(),
            year: state.get_combo_overwrite_year(),
            track: state.get_combo_overwrite_track(),
            disc: state.get_combo_overwrite_disc(),
            genre: state.get_combo_overwrite_genre(),
            comment: state.get_combo_overwrite_comment(),
            lyrics: state.get_combo_overwrite_lyrics(),
            cover: state.get_combo_overwrite_cover(),
            write_163_key: state.get_opt_write_163_key(),
        }
    }
}

pub struct BackendController {
    instance_id: u64,
    config: AppConfig,
    history_store: HistoryStore,
    format_registry: Arc<FormatRegistry>,
    source_registry: SourceRegistry,

    disk_tracks: Vec<AudioTrack>,
    working_tracks: Vec<AudioTrack>,
    staged_patches: Vec<TagPatch>,
    staged_cover_previews: Vec<Option<CoverData>>,
    extra_modified: Vec<bool>,
    extra_instrumental: Vec<Option<bool>>,
    selected_flags: Vec<bool>,
    ui_tracks: Vec<TrackItem>,
    filtered_indices: Vec<usize>,

    explicit_files: Vec<PathBuf>,
    last_combo_candidates: Vec<CandidateTrack>,
    last_cover_candidates: Vec<CoverSearchItem>,
    last_history_records: Vec<TagHistoryRecord>,

    scan_gen: u64,
    search_gen: u64,
    active_rg_cancel: Option<Arc<AtomicBool>>,
    active_am_cancel: Option<Arc<AtomicBool>>,
    find_replace_from_editor: bool,
    adjust_from_editor: bool,
    find_cursor: usize,

    live_preview_timer: Option<slint::Timer>,
    last_fn_preview_sig: Option<String>,
    last_charset_preview_sig: Option<(i32, Option<usize>, String)>,

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

pub fn launch(ui: &AppWindow, cli_paths: Vec<PathBuf>) -> Result<(), Box<dyn Error>> {
    let instance_id = alloc_instance_id();

    let mut config = match AppConfig::load_default() {
        Ok(c) => c,
        Err(err) => {
            eprintln!("Warning: failed to load default config: {err:#}");
            AppConfig::default()
        }
    };

    let (history_store, history_warn) = match HistoryStore::open_default() {
        Ok(store) => (store, None),
        Err(err) => {
            eprintln!("Warning: failed to open default history DB: {err:#}");
            (
                HistoryStore::open_in_memory()?,
                Some("历史数据库不可写，已启用临时内存记录"),
            )
        }
    };

    let format_registry = Arc::new(build_default_format_registry());
    let plugin_dirs = plugin_search_dirs();
    let mut source_registry = build_default_source_registry(&plugin_dirs, &config.artist_sep);
    sync_registry_with_config(&mut source_registry, &config);

    // Apply CLI paths (if provided) without overwriting saved library directories on disk.
    let mut explicit_files = Vec::new();
    let mut cli_dirs = Vec::new();
    for raw in cli_paths {
        let abs = std::fs::canonicalize(&raw).unwrap_or(raw);
        if abs.is_dir() {
            cli_dirs.push(DirConfigEntry {
                path: abs.to_string_lossy().into_owned(),
                include_subdirs: true,
            });
        } else if abs.is_file() {
            explicit_files.push(abs);
        }
    }

    if !cli_dirs.is_empty() || !explicit_files.is_empty() {
        config.dirs = cli_dirs;
        if let Some(first_dir) = config.dirs.first() {
            config.working_dir = first_dir.path.clone();
        } else if let Some(first_file) = explicit_files.first() {
            config.working_dir = first_file
                .parent()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
        }
    } else if config.dirs.is_empty() && !config.working_dir.trim().is_empty() {
        let wd = PathBuf::from(&config.working_dir);
        if wd.is_dir() {
            config.dirs.push(DirConfigEntry {
                path: config.working_dir.clone(),
                include_subdirs: true,
            });
        }
    }

    let columns_init = build_columns_from_config(&config.columns);
    let dirs_init: Vec<DirEntry> = config
        .dirs
        .iter()
        .map(|d| DirEntry {
            path: d.path.clone().into(),
            include_subdirs: d.include_subdirs,
            file_count: 0,
        })
        .collect();
    let sources_init: Vec<SourceItem> = source_registry
        .list()
        .iter()
        .map(source_info_to_item)
        .collect();

    let (history_records, history_entries) = load_history_snapshot(&history_store);

    let ctrl = Rc::new(RefCell::new(BackendController {
        instance_id,
        config,
        history_store,
        format_registry,
        source_registry,
        disk_tracks: Vec::new(),
        working_tracks: Vec::new(),
        staged_patches: Vec::new(),
        staged_cover_previews: Vec::new(),
        extra_modified: Vec::new(),
        extra_instrumental: Vec::new(),
        selected_flags: Vec::new(),
        ui_tracks: Vec::new(),
        filtered_indices: Vec::new(),
        explicit_files,
        last_combo_candidates: Vec::new(),
        last_cover_candidates: Vec::new(),
        last_history_records: history_records,
        scan_gen: 0,
        search_gen: 0,
        active_rg_cancel: None,
        active_am_cancel: None,
        find_replace_from_editor: false,
        adjust_from_editor: false,
        find_cursor: 0,
        live_preview_timer: None,
        last_fn_preview_sig: None,
        last_charset_preview_sig: None,
        tracks_model: Rc::new(VecModel::default()),
        candidates_model: Rc::new(VecModel::default()),
        covers_model: Rc::new(VecModel::default()),
        columns_model: Rc::new(VecModel::from(columns_init)),
        dirs_model: Rc::new(VecModel::from(dirs_init)),
        history_model: Rc::new(VecModel::from(history_entries)),
        regex_groups_model: Rc::new(VecModel::from(default_regex_groups())),
        filename_previews_model: Rc::new(VecModel::default()),
        auto_match_model: Rc::new(VecModel::default()),
        charset_model: Rc::new(VecModel::default()),
        sources_model: Rc::new(VecModel::from(sources_init)),
    }));

    register_controller(instance_id, &ctrl);

    {
        let mut c = ctrl.borrow_mut();
        init_ui_state(ui, &c);
        if let Some(warn) = history_warn {
            ui.global::<AppState>().set_status_message(warn.into());
        }
        start_live_preview_timer(ui, &mut c);
    }

    wire_callbacks(ui, ctrl.clone());

    {
        let mut c = ctrl.borrow_mut();
        if !c.config.dirs.is_empty() || !c.explicit_files.is_empty() {
            c.spawn_directory_scan(ui);
        } else {
            ui.global::<AppState>()
                .set_status_message("就绪 · 请点击「改变目录」或「添加目录」加载音频文件".into());
        }
    }

    Ok(())
}

pub fn plugin_search_dirs() -> Vec<PathBuf> {
    let mut dirs_out = Vec::new();
    let mut seen = HashSet::new();

    let mut push_if_unique = |path: PathBuf| {
        let key = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if seen.insert(key) {
            dirs_out.push(path);
        }
    };

    if let Ok(cwd) = std::env::current_dir() {
        push_if_unique(cwd.join("plugins").join("sources"));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            push_if_unique(parent.join("plugins").join("sources"));
        }
    }
    if let Some(cfg_dir) = dirs::config_dir() {
        push_if_unique(
            cfg_dir
                .join("musictag-native")
                .join("plugins")
                .join("sources"),
        );
    }

    dirs_out
}

pub fn sync_registry_with_config(registry: &mut SourceRegistry, cfg: &AppConfig) {
    let known_in_priority: HashSet<&str> = cfg.source_priority.iter().map(String::as_str).collect();
    let enabled_set: HashSet<&str> = cfg.enabled_sources.iter().map(String::as_str).collect();

    for info in registry.list() {
        if known_in_priority.contains(info.id.as_str()) {
            registry.set_enabled(&info.id, enabled_set.contains(info.id.as_str()));
        }
    }

    if !cfg.source_priority.is_empty() {
        registry.reorder(&cfg.source_priority);
    }
}

fn source_info_to_item(info: &SourceInfo) -> SourceItem {
    SourceItem {
        id: info.id.clone().into(),
        name: info.name.clone().into(),
        version: info.version.clone().into(),
        description: info.description.clone().into(),
        builtin: info.builtin,
        enabled: info.enabled,
        supports_combo: info.capabilities.combo,
        supports_lyrics: info.capabilities.lyrics,
        supports_cover: info.capabilities.cover,
        supports_163: info.capabilities.key163,
    }
}

fn default_columns() -> Vec<ColumnConfigItem> {
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

fn build_columns_from_config(saved: &[ColumnConfigEntry]) -> Vec<ColumnConfigItem> {
    let defaults = default_columns();
    if saved.is_empty() {
        return defaults;
    }

    let mut by_key: HashMap<String, ColumnConfigItem> = defaults
        .iter()
        .map(|c| (c.key.to_string(), c.clone()))
        .collect();
    let mut out = Vec::new();
    for entry in saved {
        if let Some(mut item) = by_key.remove(&entry.key) {
            item.visible = entry.visible;
            if entry.width > 0 {
                item.width = entry.width;
            }
            out.push(item);
        }
    }
    for col in defaults {
        if let Some(missing) = by_key.remove(col.key.as_str()) {
            out.push(missing);
        }
    }
    out
}

fn default_regex_groups() -> Vec<RegexGroupItem> {
    vec![
        RegexGroupItem { group_index: 1, target_field: "@5 - 音轨号".into() },
        RegexGroupItem { group_index: 2, target_field: "@2 - 艺术家".into() },
        RegexGroupItem { group_index: 3, target_field: "@1 - 标题".into() },
    ]
}

fn load_history_snapshot(store: &HistoryStore) -> (Vec<TagHistoryRecord>, Vec<HistoryEntry>) {
    let records = store.list(200).unwrap_or_default();
    let entries = records
        .iter()
        .map(|r| HistoryEntry {
            timestamp: r.timestamp.clone().into(),
            summary: r.summary.clone().into(),
            title: r.title.clone().into(),
            artist: r.artist.clone().into(),
            album: r.album.clone().into(),
            comment_has_163: r.comment_has_163,
        })
        .collect();
    (records, entries)
}

fn theme_pref_from_i32(pref: i32) -> ThemePreference {
    match pref {
        0 => ThemePreference::System,
        1 => ThemePreference::Light,
        _ => ThemePreference::Dark,
    }
}

fn theme_pref_to_i32(pref: ThemePreference) -> i32 {
    match pref {
        ThemePreference::System => 0,
        ThemePreference::Light => 1,
        ThemePreference::Dark => 2,
    }
}

fn init_ui_state(ui: &AppWindow, c: &BackendController) {
    let state = ui.global::<AppState>();
    state.set_tracks(ModelRc::from(c.tracks_model.clone()));
    state.set_search_candidates(ModelRc::from(c.candidates_model.clone()));
    state.set_cover_candidates(ModelRc::from(c.covers_model.clone()));
    state.set_columns(ModelRc::from(c.columns_model.clone()));
    state.set_dirs(ModelRc::from(c.dirs_model.clone()));
    state.set_history(ModelRc::from(c.history_model.clone()));
    state.set_regex_groups(ModelRc::from(c.regex_groups_model.clone()));
    state.set_filename_previews(ModelRc::from(c.filename_previews_model.clone()));
    state.set_auto_match_rows(ModelRc::from(c.auto_match_model.clone()));
    state.set_charset_rows(ModelRc::from(c.charset_model.clone()));
    state.set_source_items(ModelRc::from(c.sources_model.clone()));
    state.set_selected_source_index(0);
    state.set_selected_index(-1);
    state.set_selected_count(0);
    state.set_modified_count(0);
    state.set_progress_title("就绪".into());
    state.set_progress_file("".into());
    state.set_progress_value(0.0);
    state.set_progress_ok(0);
    state.set_progress_fail(0);
    state.set_progress_skip(0);
    state.set_progress_total(0);

    apply_config_to_ui(ui, &c.config);
}

fn apply_config_to_ui(ui: &AppWindow, cfg: &AppConfig) {
    ui.global::<Theme>()
        .set_preference(theme_pref_from_i32(cfg.theme_preference));

    let state = ui.global::<AppState>();
    state.set_working_dir(if cfg.working_dir.is_empty() {
        "未设置工作目录".into()
    } else {
        cfg.working_dir.clone().into()
    });

    state.set_opt_write_163_key(cfg.write_163_key);
    state.set_opt_artist_sep(cfg.artist_sep.clone().into());
    state.set_opt_id3v2_ver(cfg.id3v2_ver_index());

    state.set_opt_dl_trans(cfg.dl_trans);
    state.set_opt_dont_dl_orig(cfg.dont_dl_orig);
    state.set_opt_trans_format(cfg.trans_format as i32);
    state.set_opt_trans_connector(cfg.trans_connector.clone().into());
    state.set_opt_delete_head_tag(cfg.delete_head_tag);
    state.set_opt_delete_blank_lines(cfg.delete_blank_lines);
    state.set_opt_reformat_timetag(cfg.reformat_timetag);
    state.set_opt_remove_timetag(cfg.remove_timetag);
    state.set_opt_skip_instrumental(cfg.skip_instrumental);
    state.set_opt_zh_conv(cfg.zh_conv as i32);

    state.set_opt_rg_mode(cfg.rg_mode as i32);
    state.set_opt_rg_target_lufs(format!("{:.1}", cfg.rg_target_lufs).into());
    state.set_opt_rg_clip_prevent(cfg.rg_clip_prevent);
    state.set_opt_rg_lowercase_flac(cfg.rg_lowercase_flac);

    state.set_opt_save_lrc_with_tag(cfg.save_lrc_with_tag);
    state.set_opt_lrc_dir(cfg.lrc_dir.clone().into());
    state.set_opt_lrc_naming(cfg.lrc_naming as i32);
    state.set_opt_lrc_encoding(cfg.lrc_encoding.clone().into());
    state.set_opt_keep_mtime(cfg.keep_mtime);
    state.set_opt_pic_res_limit(cfg.pic_res_limit.to_string().into());
    state.set_opt_pic_size_limit(cfg.pic_size_limit_kb.to_string().into());
    state.set_opt_pic_format(cfg.pic_format as i32);
    state.set_opt_overwrite_cover(cfg.overwrite_cover);
    state.set_opt_ignore_video(cfg.ignore_video);
    state.set_opt_duration_filter(cfg.duration_filter as i32);
    state.set_opt_file_exts(cfg.file_exts.clone().into());

    state.set_opt_search_cond_title_mode(cfg.search_cond_title_mode as i32);
    state.set_opt_search_use_artist(cfg.search_use_artist);
    state.set_opt_search_use_album(cfg.search_use_album);
    state.set_opt_search_limit(cfg.search_limit as i32);
    state.set_opt_thread_count(cfg.thread_count.clamp(1, 16) as i32);
    state.set_opt_minimize_tray(cfg.minimize_tray);
    state.set_opt_always_tray(cfg.always_tray);
    state.set_opt_check_updates(cfg.check_updates);
}

fn read_config_from_ui(ui: &AppWindow, c: &BackendController) -> AppConfig {
    let state = ui.global::<AppState>();
    let mut cfg = c.config.clone();

    cfg.theme_preference = theme_pref_to_i32(ui.global::<Theme>().get_preference());
    cfg.write_163_key = state.get_opt_write_163_key();
    let sep = state.get_opt_artist_sep().to_string();
    cfg.artist_sep = if sep.is_empty() { ";".to_string() } else { sep };
    cfg.set_id3v2_ver_index(state.get_opt_id3v2_ver());

    cfg.dl_trans = state.get_opt_dl_trans();
    cfg.dont_dl_orig = state.get_opt_dont_dl_orig();
    cfg.trans_format = state.get_opt_trans_format().clamp(0, 3) as u8;
    cfg.trans_connector = state.get_opt_trans_connector().to_string();
    cfg.delete_head_tag = state.get_opt_delete_head_tag();
    cfg.delete_blank_lines = state.get_opt_delete_blank_lines();
    cfg.reformat_timetag = state.get_opt_reformat_timetag();
    cfg.remove_timetag = state.get_opt_remove_timetag();
    cfg.skip_instrumental = state.get_opt_skip_instrumental();
    cfg.zh_conv = state.get_opt_zh_conv().clamp(0, 2) as u8;

    cfg.rg_mode = state.get_opt_rg_mode().clamp(0, 1) as u8;
    if let Ok(lufs) = state.get_opt_rg_target_lufs().trim().parse::<f64>() {
        if lufs.is_finite() {
            cfg.rg_target_lufs = lufs;
        }
    }
    cfg.rg_clip_prevent = state.get_opt_rg_clip_prevent();
    cfg.rg_lowercase_flac = state.get_opt_rg_lowercase_flac();

    cfg.save_lrc_with_tag = state.get_opt_save_lrc_with_tag();
    cfg.lrc_dir = state.get_opt_lrc_dir().to_string();
    cfg.lrc_naming = state.get_opt_lrc_naming().clamp(0, 2) as u8;
    cfg.lrc_encoding = state.get_opt_lrc_encoding().to_string();
    cfg.keep_mtime = state.get_opt_keep_mtime();
    cfg.pic_res_limit = state.get_opt_pic_res_limit().trim().parse::<u32>().unwrap_or(0);
    cfg.pic_size_limit_kb = state
        .get_opt_pic_size_limit()
        .trim()
        .parse::<u32>()
        .unwrap_or(2000);
    cfg.pic_format = state.get_opt_pic_format().clamp(0, 2) as u8;
    cfg.overwrite_cover = state.get_opt_overwrite_cover();
    cfg.ignore_video = state.get_opt_ignore_video();
    cfg.duration_filter = state.get_opt_duration_filter().clamp(0, 3) as u8;
    cfg.file_exts = state.get_opt_file_exts().to_string();

    cfg.search_cond_title_mode = state.get_opt_search_cond_title_mode().clamp(0, 1) as u8;
    cfg.search_use_artist = state.get_opt_search_use_artist();
    cfg.search_use_album = state.get_opt_search_use_album();
    cfg.search_limit = state.get_opt_search_limit().clamp(1, 200) as usize;
    cfg.thread_count = state.get_opt_thread_count().clamp(1, 16) as usize;
    cfg.minimize_tray = state.get_opt_minimize_tray();
    cfg.always_tray = state.get_opt_always_tray();
    cfg.check_updates = state.get_opt_check_updates();

    let sources = c.source_registry.list();
    cfg.source_priority = sources.iter().map(|s| s.id.clone()).collect();
    cfg.enabled_sources = sources
        .iter()
        .filter(|s| s.enabled)
        .map(|s| s.id.clone())
        .collect();

    cfg.columns = (0..c.columns_model.row_count())
        .filter_map(|i| c.columns_model.row_data(i))
        .map(|col| ColumnConfigEntry {
            key: col.key.to_string(),
            visible: col.visible,
            width: col.width,
        })
        .collect();

    cfg
}

fn start_live_preview_timer(ui: &AppWindow, c: &mut BackendController) {
    let timer = slint::Timer::default();
    let ui_w = ui.as_weak();
    let instance_id = c.instance_id;
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(120),
        move || {
            let Some(ui) = ui_w.upgrade() else { return };
            let dlg = ui.global::<AppState>().get_active_dialog();
            if dlg != ActiveDialog::FilenameRel && dlg != ActiveDialog::CharsetFix {
                return;
            }
            with_controller(instance_id, |c| {
                match dlg {
                    ActiveDialog::FilenameRel => {
                        c.refresh_filename_previews_if_changed(&ui, false);
                    }
                    ActiveDialog::CharsetFix => {
                        c.refresh_charset_preview_if_changed(&ui, false);
                    }
                    _ => {}
                }
            });
        },
    );
    c.live_preview_timer = Some(timer);
}

impl BackendController {
    fn selected_master_index(&self, ui: &AppWindow) -> Option<usize> {
        let idx = ui.global::<AppState>().get_selected_index();
        if idx < 0 {
            return None;
        }
        self.filtered_indices.get(idx as usize).copied()
    }

    fn checked_master_indices(&self) -> Vec<usize> {
        self.selected_flags
            .iter()
            .enumerate()
            .filter_map(|(i, &sel)| sel.then_some(i))
            .collect()
    }

    fn edit_target_master_indices(&self, ui: &AppWindow) -> Vec<usize> {
        let checked = self.checked_master_indices();
        if checked.len() > 1 {
            checked
        } else {
            self.selected_master_index(ui).into_iter().collect()
        }
    }

    fn batch_target_master_indices(&self, ui: &AppWindow) -> Vec<usize> {
        let checked = self.checked_master_indices();
        if !checked.is_empty() {
            checked
        } else {
            self.selected_master_index(ui).into_iter().collect()
        }
    }

    fn is_track_modified(&self, m_idx: usize) -> bool {
        !self.staged_patches[m_idx].is_empty() || self.extra_modified[m_idx]
    }

    fn refresh_track_row(&mut self, m_idx: usize) {
        let mut derived = apply_patch_to_track(
            &self.disk_tracks[m_idx],
            &self.staged_patches[m_idx],
            self.staged_cover_previews[m_idx].as_ref(),
        );
        if let Some(inst) = self.extra_instrumental[m_idx] {
            derived.is_instrumental = inst;
        }
        let modified = self.is_track_modified(m_idx);
        let selected = self.selected_flags[m_idx];
        let item = audio_track_to_track_item((m_idx + 1) as i32, &derived, modified, selected);
        self.working_tracks[m_idx] = derived;
        self.ui_tracks[m_idx] = item;

        if let Some(v_idx) = self.filtered_indices.iter().position(|&idx| idx == m_idx) {
            if v_idx < self.tracks_model.row_count() {
                self.tracks_model
                    .set_row_data(v_idx, self.ui_tracks[m_idx].clone());
            }
        }
    }

    fn sync_visible_tracks(&self) {
        for (v_idx, &m_idx) in self.filtered_indices.iter().enumerate() {
            if v_idx < self.tracks_model.row_count() {
                self.tracks_model
                    .set_row_data(v_idx, self.ui_tracks[m_idx].clone());
            }
        }
    }

    fn sync_counts_and_status(&self, ui: &AppWindow, extra: Option<&str>) {
        let state = ui.global::<AppState>();
        let selected_count = self.selected_flags.iter().filter(|&&s| s).count() as i32;
        let modified_count = (0..self.working_tracks.len())
            .filter(|&i| self.is_track_modified(i))
            .count() as i32;
        state.set_selected_count(selected_count);
        state.set_modified_count(modified_count);

        let visible_total = self.filtered_indices.len();
        let total = self.working_tracks.len();
        let base = if total == 0 {
            "共 0 首曲目".to_string()
        } else if visible_total == total {
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
            let track = &self.ui_tracks[m_idx];
            state.set_lyric_editor_text(track.lyrics.clone());
        } else {
            state.set_lyric_editor_text("".into());
        }

        if checked.len() > 1 {
            state.set_is_multi_select(true);

            for &field in TEXT_FIELDS {
                let first_mode = patch_field_mode(&self.staged_patches[checked[0]], field);
                let first_val = ui_track_text_field(&self.ui_tracks[checked[0]], field);
                let all_same_mode = checked
                    .iter()
                    .all(|&i| patch_field_mode(&self.staged_patches[i], field) == first_mode);
                let all_same_val = checked
                    .iter()
                    .all(|&i| ui_track_text_field(&self.ui_tracks[i], field) == first_val);

                let insp = if all_same_mode && first_mode == 2 {
                    InspectorField {
                        text: "".into(),
                        mode: 2,
                        mixed: false,
                    }
                } else if all_same_val {
                    let mode = if all_same_mode && first_mode == 0 { 0 } else { 1 };
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

            let all_clear_cover = checked
                .iter()
                .all(|&i| self.staged_patches[i].cover.is_clear());
            let first_t = &self.ui_tracks[checked[0]];
            let all_same_cover = checked.iter().all(|&i| {
                let t = &self.ui_tracks[i];
                t.has_picture == first_t.has_picture
                    && t.cover_info == first_t.cover_info
                    && self.working_tracks[i].cover.as_ref().map(|c| &c.bytes)
                        == self.working_tracks[checked[0]].cover.as_ref().map(|c| &c.bytes)
            });
            if all_clear_cover {
                state.set_field_cover_mode(2);
                state.set_field_cover_mixed(false);
            } else {
                let all_set = checked
                    .iter()
                    .all(|&i| self.staged_patches[i].cover.is_set());
                state.set_field_cover_mode(if all_set && all_same_cover { 0 } else { 1 });
                state.set_field_cover_mixed(!all_same_cover);
            }

            let all_clear_lyrics = checked
                .iter()
                .all(|&i| self.staged_patches[i].lyrics.is_clear());
            let all_same_lyrics = checked.iter().all(|&i| {
                let t = &self.ui_tracks[i];
                t.has_lyrics == first_t.has_lyrics
                    && t.is_instrumental == first_t.is_instrumental
                    && t.lyrics == first_t.lyrics
            });
            if all_clear_lyrics {
                state.set_field_lyrics_mode(2);
                state.set_field_lyrics_mixed(false);
            } else {
                let all_set = checked
                    .iter()
                    .all(|&i| self.staged_patches[i].lyrics.is_set());
                state.set_field_lyrics_mode(if all_set && all_same_lyrics { 0 } else { 1 });
                state.set_field_lyrics_mixed(!all_same_lyrics);
            }
        } else {
            state.set_is_multi_select(false);

            if let Some(m_idx) = self.selected_master_index(ui) {
                let track = &self.ui_tracks[m_idx];
                let patch = &self.staged_patches[m_idx];

                for &field in TEXT_FIELDS {
                    let mode = patch_field_mode(patch, field);
                    let cur_val = ui_track_text_field(track, field);
                    set_inspector_field(
                        &state,
                        field,
                        InspectorField {
                            text: if mode == 2 { "".into() } else { cur_val },
                            mode,
                            mixed: false,
                        },
                    );
                }

                let cover_mode = if patch.cover.is_clear() {
                    2
                } else if patch.cover.is_set() {
                    0
                } else {
                    1
                };
                state.set_field_cover_mode(cover_mode);
                state.set_field_cover_mixed(false);

                let lyrics_mode = if patch.lyrics.is_clear() {
                    2
                } else if patch.lyrics.is_set() {
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

    fn rebuild_filtered_tracks(&mut self, ui: &AppWindow, field_idx: i32, keyword: &str) {
        let prev_selected_path = self
            .selected_master_index(ui)
            .map(|m| self.working_tracks[m].path.clone());

        let kw = keyword.trim().to_lowercase();
        self.filtered_indices.clear();
        let mut visible = Vec::new();

        for (i, t) in self.ui_tracks.iter().enumerate() {
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
                            || contains(&t.album_artist)
                            || contains(&t.album)
                            || contains(&t.genre)
                            || contains(&t.year)
                            || contains(&t.tag_types)
                            || contains(&t.netease_id)
                            || contains(&t.comment)
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
        } else if let Some(prev_path) = prev_selected_path {
            if let Some(new_v) = self
                .filtered_indices
                .iter()
                .position(|&m| self.working_tracks[m].path == prev_path)
            {
                state.set_selected_index(new_v as i32);
            } else {
                state.set_selected_index(0);
            }
        } else {
            state.set_selected_index(0);
        }

        self.sync_selected_track_inputs(ui);
        self.sync_counts_and_status(ui, None);
    }

    fn refresh_sources_model(&self, ui: &AppWindow) {
        let items: Vec<SourceItem> = self
            .source_registry
            .list()
            .iter()
            .map(source_info_to_item)
            .collect();
        let len = items.len();
        self.sources_model.set_vec(items);
        let state = ui.global::<AppState>();
        let sel = state.get_selected_source_index();
        if len == 0 {
            state.set_selected_source_index(-1);
        } else if sel < 0 || (sel as usize) >= len {
            state.set_selected_source_index(0);
        }
    }

    fn reload_history_from_db(&mut self) {
        let (records, entries) = load_history_snapshot(&self.history_store);
        self.last_history_records = records;
        self.history_model.set_vec(entries);
    }

    fn sync_dirs_model(&self, ui: &AppWindow, counts: Option<&[usize]>) {
        let entries: Vec<DirEntry> = self
            .config
            .dirs
            .iter()
            .enumerate()
            .map(|(i, d)| DirEntry {
                path: d.path.clone().into(),
                include_subdirs: d.include_subdirs,
                file_count: counts.and_then(|c| c.get(i).copied()).unwrap_or(0) as i32,
            })
            .collect();
        self.dirs_model.set_vec(entries);

        let wd = if !self.config.working_dir.is_empty() {
            self.config.working_dir.clone()
        } else if let Some(first) = self.config.dirs.first() {
            first.path.clone()
        } else {
            "未设置工作目录".to_string()
        };
        ui.global::<AppState>().set_working_dir(wd.into());
    }

    fn spawn_directory_scan(&mut self, ui: &AppWindow) {
        self.scan_gen = self.scan_gen.wrapping_add(1);
        let gen_id = self.scan_gen;
        let instance_id = self.instance_id;
        let ui_w = ui.as_weak();

        let cfg_snapshot = read_config_from_ui(ui, self);
        let dirs_snapshot = self.config.dirs.clone();
        let explicit_snapshot = self.explicit_files.clone();
        let format_reg = Arc::clone(&self.format_registry);
        let allowed_exts = cfg_snapshot.allowed_extensions();
        let ignore_video = cfg_snapshot.ignore_video;
        let duration_filter = cfg_snapshot.duration_filter;

        ui.global::<AppState>()
            .set_status_message("正在扫描目录并读取音频标签...".into());

        std::thread::spawn(move || {
            let (paths, dir_counts) = collect_audio_files(
                &dirs_snapshot,
                &explicit_snapshot,
                &format_reg,
                &allowed_exts,
                ignore_video,
            );

            let min_duration_ms = match duration_filter {
                1 => 15_000,
                2 => 30_000,
                3 => 60_000,
                _ => 0,
            };

            let results: Vec<Result<AudioTrack>> = paths
                .par_iter()
                .map(|path| format_reg.read(path))
                .collect();

            let mut tracks = Vec::with_capacity(results.len());
            let mut failed_count = 0usize;
            for res in results {
                match res {
                    Ok(track) => {
                        if min_duration_ms == 0 || track.duration_ms >= min_duration_ms {
                            tracks.push(track);
                        }
                    }
                    Err(err) => {
                        failed_count += 1;
                        eprintln!("Failed to read audio file: {err:#}");
                    }
                }
            }

            tracks.sort_by(|a, b| a.path.cmp(&b.path));

            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = ui_w.upgrade() else { return };
                with_controller(instance_id, |c| {
                    if c.scan_gen != gen_id {
                        return;
                    }
                    c.finish_directory_scan(&ui, tracks, dir_counts, failed_count);
                });
            });
        });
    }

    fn finish_directory_scan(
        &mut self,
        ui: &AppWindow,
        tracks: Vec<AudioTrack>,
        dir_counts: Vec<usize>,
        failed_count: usize,
    ) {
        let count = tracks.len();
        self.disk_tracks = tracks.clone();
        self.working_tracks = tracks;
        self.staged_patches = vec![TagPatch::default(); count];
        self.staged_cover_previews = vec![None; count];
        self.extra_modified = vec![false; count];
        self.extra_instrumental = vec![None; count];
        self.selected_flags = vec![false; count];
        self.ui_tracks = self
            .working_tracks
            .iter()
            .enumerate()
            .map(|(i, t)| audio_track_to_track_item((i + 1) as i32, t, false, false))
            .collect();

        self.sync_dirs_model(ui, Some(&dir_counts));

        let state = ui.global::<AppState>();
        let field_idx = state.get_filter_field_index();
        let keyword = state.get_filter_keyword().to_string();
        self.rebuild_filtered_tracks(ui, field_idx, &keyword);

        let note = if failed_count > 0 {
            format!("已加载 {count} 首曲目 ({failed_count} 个文件读取失败)")
        } else {
            format!("已加载 {count} 首曲目")
        };
        self.sync_counts_and_status(ui, Some(&note));
        if state.get_active_dialog() == ActiveDialog::None {
            ui.invoke_focus_main();
        }
    }

    fn edit_selected_field(&mut self, ui: &AppWindow, field: &str, value: SharedString) {
        let targets = self.edit_target_master_indices(ui);
        if targets.is_empty() {
            return;
        }
        let multi = targets.len() > 1;
        let state = ui.global::<AppState>();

        match field {
            "netease-id" => {
                for &m_idx in &targets {
                    self.working_tracks[m_idx].netease_id = value.to_string();
                    self.ui_tracks[m_idx].netease_id = value.clone();
                    self.extra_modified[m_idx] =
                        self.working_tracks[m_idx].netease_id != self.disk_tracks[m_idx].netease_id;
                    self.refresh_track_row(m_idx);
                }
            }
            "lyrics" => {
                for &m_idx in &targets {
                    let s = value.to_string();
                    self.staged_patches[m_idx].lyrics = if !multi && s == self.disk_tracks[m_idx].lyrics {
                        FieldUpdate::Keep
                    } else {
                        FieldUpdate::Set(s)
                    };
                    self.extra_instrumental[m_idx] = None;
                    self.refresh_track_row(m_idx);
                }
                state.set_field_lyrics_mode(0);
                state.set_field_lyrics_mixed(false);
            }
            "is-instrumental" => {
                let inst = value == "true" || value == "1";
                for &m_idx in &targets {
                    self.extra_instrumental[m_idx] = Some(inst);
                    self.extra_modified[m_idx] = inst != self.disk_tracks[m_idx].is_instrumental;
                    self.refresh_track_row(m_idx);
                }
            }
            _ => {
                if !TEXT_FIELDS.contains(&field) {
                    return;
                }
                let val_str = value.to_string();
                let mut resulting_mode = 0;
                for &m_idx in &targets {
                    let disk_val = track_text_field(&self.disk_tracks[m_idx], field);
                    let update = if !multi && val_str == disk_val {
                        resulting_mode = 1;
                        FieldUpdate::Keep
                    } else {
                        resulting_mode = 0;
                        FieldUpdate::Set(val_str.clone())
                    };
                    set_patch_text_field(&mut self.staged_patches[m_idx], field, update);
                    self.refresh_track_row(m_idx);
                }
                set_inspector_field(
                    &state,
                    field,
                    InspectorField {
                        text: value,
                        mode: resulting_mode,
                        mixed: false,
                    },
                );
            }
        }

        self.sync_counts_and_status(ui, Some(&format!("已编辑字段: {field}")));
    }

    fn set_field_mode(&mut self, ui: &AppWindow, field: &str, mode: i32) {
        let targets = self.edit_target_master_indices(ui);
        if targets.is_empty() {
            return;
        }
        let normalized_mode = if mode == 2 { 2 } else { 1 };
        let state = ui.global::<AppState>();

        for &m_idx in &targets {
            match field {
                "cover" => {
                    self.staged_patches[m_idx].cover = if normalized_mode == 2 {
                        FieldUpdate::Clear
                    } else {
                        self.staged_cover_previews[m_idx] = None;
                        FieldUpdate::Keep
                    };
                }
                "lyrics" => {
                    self.staged_patches[m_idx].lyrics = if normalized_mode == 2 {
                        FieldUpdate::Clear
                    } else {
                        FieldUpdate::Keep
                    };
                    self.extra_instrumental[m_idx] = None;
                }
                _ => {
                    if !TEXT_FIELDS.contains(&field) {
                        return;
                    }
                    let update = if normalized_mode == 2 {
                        FieldUpdate::Clear
                    } else {
                        FieldUpdate::Keep
                    };
                    set_patch_text_field(&mut self.staged_patches[m_idx], field, update);
                }
            }
            self.refresh_track_row(m_idx);
        }

        self.sync_selected_track_inputs(ui);

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
        self.sync_counts_and_status(ui, Some(&format!("字段 {field} 已设为{action_label}")));
    }

    fn save_selected(&mut self, ui: &AppWindow) {
        let any_checked_mod = (0..self.working_tracks.len())
            .any(|i| self.selected_flags[i] && self.is_track_modified(i));

        let indices: Vec<usize> = (0..self.working_tracks.len())
            .filter(|&i| self.is_track_modified(i) && (!any_checked_mod || self.selected_flags[i]))
            .collect();

        if indices.is_empty() {
            self.sync_counts_and_status(ui, Some("没有需要保存的标签修改"));
            return;
        }

        let cfg = read_config_from_ui(ui, self);
        let write_opts = cfg.to_write_options();
        let mut saved = 0usize;
        let mut errors = Vec::new();

        for m_idx in indices {
            let path = self.disk_tracks[m_idx].path.clone();
            let patch = self.staged_patches[m_idx].clone();

            if !patch.is_empty() {
                // Record initial snapshot if this track has no history yet so original tags remain recoverable.
                let path_str = path.to_string_lossy().into_owned();
                if self
                    .history_store
                    .list_for_track(&path_str, 1)
                    .map(|rows| rows.is_empty())
                    .unwrap_or(false)
                {
                    let _ = self
                        .history_store
                        .record_track(&self.disk_tracks[m_idx], "修改前原始标签快照");
                }

                if let Err(err) = self.format_registry.write_patch(&path, &patch, &write_opts) {
                    errors.push(format!("{}: {err}", self.disk_tracks[m_idx].filename));
                    continue;
                }
            }

            let fresh = match self.format_registry.read(&path) {
                Ok(t) => t,
                Err(err) => {
                    errors.push(format!("{}: 重读失败 ({err})", self.disk_tracks[m_idx].filename));
                    continue;
                }
            };

            if cfg.save_lrc_with_tag && !fresh.lyrics.trim().is_empty() {
                if let Err(err) = export_lrc_for_track(&fresh, &fresh.lyrics, &cfg) {
                    eprintln!("LRC export warning for {}: {err:#}", fresh.filename);
                }
            }

            if !patch.is_empty() {
                let summary = format!("保存标签变更 · {}", fresh.filename);
                let _ = self.history_store.record_track(&fresh, &summary);
            }

            self.disk_tracks[m_idx] = fresh;
            self.staged_patches[m_idx] = TagPatch::default();
            self.staged_cover_previews[m_idx] = None;
            self.extra_modified[m_idx] = false;
            self.extra_instrumental[m_idx] = None;
            self.refresh_track_row(m_idx);
            saved += 1;
        }

        self.reload_history_from_db();
        self.sync_selected_track_inputs(ui);

        if errors.is_empty() {
            self.sync_counts_and_status(ui, Some(&format!("已成功保存 {saved} 首曲目标签")));
        } else {
            self.sync_counts_and_status(
                ui,
                Some(&format!(
                    "已保存 {saved} 首 · {} 首失败 ({})",
                    errors.len(),
                    errors[0]
                )),
            );
        }
    }

    fn revert_selected(&mut self, ui: &AppWindow) {
        let targets = self.batch_target_master_indices(ui);
        if targets.is_empty() {
            return;
        }
        let mut reverted = 0usize;
        for m_idx in targets {
            if self.is_track_modified(m_idx) {
                reverted += 1;
            }
            self.staged_patches[m_idx] = TagPatch::default();
            self.staged_cover_previews[m_idx] = None;
            self.extra_modified[m_idx] = false;
            self.extra_instrumental[m_idx] = None;
            self.refresh_track_row(m_idx);
        }
        self.sync_selected_track_inputs(ui);
        self.sync_counts_and_status(ui, Some(&format!("已撤销 {reverted} 首未保存修改")));
    }

    fn reload_selected_from_disk(&mut self, ui: &AppWindow) {
        let targets = self.batch_target_master_indices(ui);
        if targets.is_empty() {
            self.spawn_directory_scan(ui);
            return;
        }
        let mut reloaded = 0usize;
        for m_idx in targets {
            let path = self.disk_tracks[m_idx].path.clone();
            if let Ok(fresh) = self.format_registry.read(&path) {
                self.disk_tracks[m_idx] = fresh;
                self.staged_patches[m_idx] = TagPatch::default();
                self.staged_cover_previews[m_idx] = None;
                self.extra_modified[m_idx] = false;
                self.extra_instrumental[m_idx] = None;
                self.refresh_track_row(m_idx);
                reloaded += 1;
            }
        }
        self.sync_selected_track_inputs(ui);
        self.sync_counts_and_status(ui, Some(&format!("已从磁盘重读 {reloaded} 首曲目标签")));
    }

    fn clear_tags_selected(&mut self, ui: &AppWindow) {
        let targets = self.batch_target_master_indices(ui);
        if targets.is_empty() {
            return;
        }
        let count = targets.len();
        for m_idx in targets {
            self.staged_patches[m_idx] = TagPatch {
                title: FieldUpdate::Clear,
                artist: FieldUpdate::Clear,
                album: FieldUpdate::Clear,
                album_artist: FieldUpdate::Clear,
                year: FieldUpdate::Clear,
                track: FieldUpdate::Clear,
                disc: FieldUpdate::Clear,
                genre: FieldUpdate::Clear,
                composer: FieldUpdate::Clear,
                lyricist: FieldUpdate::Clear,
                comment: FieldUpdate::Clear,
                cover: FieldUpdate::Clear,
                lyrics: FieldUpdate::Clear,
                replaygain: None,
            };
            self.staged_cover_previews[m_idx] = None;
            self.extra_modified[m_idx] = false;
            self.extra_instrumental[m_idx] = None;
            self.refresh_track_row(m_idx);
        }
        self.sync_selected_track_inputs(ui);
        self.sync_counts_and_status(
            ui,
            Some(&format!("已标记清除 {count} 首曲目全部标签（Ctrl+S 保存生效）")),
        );
    }

    fn spawn_combo_or_lyric_search(&mut self, ui: &AppWindow, is_lyric_only: bool) {
        self.search_gen = self.search_gen.wrapping_add(1);
        let gen_id = self.search_gen;
        let instance_id = self.instance_id;
        let ui_w = ui.as_weak();

        self.last_combo_candidates.clear();
        self.candidates_model.set_vec(Vec::new());

        let state = ui.global::<AppState>();
        state.set_selected_candidate_index(-1);
        let cfg = read_config_from_ui(ui, self);
        let lyric_opts = cfg.to_lyric_options();

        let query = SearchQuery {
            title: state.get_combo_query_title().trim().to_string(),
            artist: state.get_combo_query_artist().trim().to_string(),
            album: state.get_combo_query_album().trim().to_string(),
            song_id: {
                let raw = state.get_combo_query_id().trim().to_string();
                if raw.is_empty() { None } else { Some(raw) }
            },
            limit: cfg.search_limit.max(1),
        };

        let has_query = !query.title.is_empty()
            || !query.artist.is_empty()
            || !query.album.is_empty()
            || query.song_id.is_some();
        if !has_query {
            state.set_is_searching(false);
            self.sync_counts_and_status(ui, Some("请输入搜索关键词或网易云 ID"));
            return;
        }

        let sources = if is_lyric_only {
            self.source_registry.active_lyrics_sources()
        } else {
            self.source_registry.active_combo_sources()
        };

        if sources.is_empty() {
            state.set_is_searching(false);
            self.sync_counts_and_status(
                ui,
                Some("没有已启用的在线标签源，请在「选项 -> 标签源」中启用"),
            );
            return;
        }

        state.set_is_searching(true);
        self.sync_counts_and_status(ui, Some("正在在线搜索匹配结果..."));

        std::thread::spawn(move || {
            let mut candidates = Vec::new();
            let mut last_err: Option<String> = None;

            for src in &sources {
                let res = if let Some(ref sid) = query.song_id {
                    src.fetch_detail(sid).map(|d| vec![d]).or_else(|_| src.search_combo(&query))
                } else {
                    src.search_combo(&query)
                };
                match res {
                    Ok(mut list) if !list.is_empty() => {
                        candidates.append(&mut list);
                        if candidates.len() >= query.limit {
                            candidates.truncate(query.limit);
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(err) => {
                        last_err = Some(format!("{}: {err:#}", src.info().name));
                    }
                }
            }

            let source_map: HashMap<String, Arc<dyn TagSource>> = sources
                .iter()
                .map(|s| (s.info().id, Arc::clone(s)))
                .collect();

            // Enrich top 10 candidates with lyric previews and cover thumbnails in parallel.
            let enrich_limit = candidates.len().min(10);
            let enriched: Vec<(CandidateTrack, Option<DecodedRgba>)> = std::thread::scope(|s| {
                let handles: Vec<_> = candidates
                    .into_iter()
                    .enumerate()
                    .map(|(idx, mut cand)| {
                        let src_opt = source_map.get(&cand.source_id).cloned();
                        let lyric_opts = &lyric_opts;
                        s.spawn(move || {
                            if idx < enrich_limit {
                                if cand.raw_lrc.is_none() && !cand.song_id.is_empty() {
                                    if let Some(ref src) = src_opt {
                                        if src.info().capabilities.lyrics {
                                            if let Ok(lrc) = src.fetch_lyrics(&cand.song_id) {
                                                cand.nolyric = lrc.nolyric;
                                                cand.raw_lrc = lrc.raw_lrc;
                                                cand.raw_tlyric = lrc.raw_tlyric;
                                            }
                                        }
                                    }
                                }

                                let has_trans = cand
                                    .raw_tlyric
                                    .as_deref()
                                    .is_some_and(|t| !t.trim().is_empty());
                                cand.has_translation = has_trans;

                                match process_netease_lyrics(
                                    cand.raw_lrc.as_deref(),
                                    cand.raw_tlyric.as_deref(),
                                    cand.nolyric,
                                    &cand.title,
                                    lyric_opts,
                                ) {
                                    ProcessedLyrics::Lyrics(text) => {
                                        cand.lyric_preview = text
                                            .lines()
                                            .map(|l| l.trim_end_matches('\r'))
                                            .filter(|l| !l.trim().is_empty())
                                            .take(6)
                                            .collect::<Vec<_>>()
                                            .join("\n");
                                    }
                                    ProcessedLyrics::InstrumentalSkipped => {
                                        cand.is_instrumental = true;
                                        cand.lyric_preview = "(纯音乐 · 无歌词)".to_string();
                                    }
                                    ProcessedLyrics::Empty => {}
                                }
                            }

                            if cand.cover_size.is_empty() && cand.cover_url.is_some() {
                                cand.cover_size = "原图".to_string();
                            }

                            let thumb = if idx < enrich_limit {
                                download_candidate_thumb(&cand, src_opt.as_deref())
                            } else {
                                None
                            };

                            (cand, thumb)
                        })
                    })
                    .collect();

                handles
                    .into_iter()
                    .filter_map(|h| h.join().ok())
                    .collect()
            });

            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = ui_w.upgrade() else { return };
                with_controller(instance_id, |c| {
                    if c.search_gen != gen_id {
                        return;
                    }
                    ui.global::<AppState>().set_is_searching(false);
                    let mut models = Vec::with_capacity(enriched.len());
                    let mut raw_list = Vec::with_capacity(enriched.len());
                    for (cand, thumb) in enriched {
                        let cover_img = decoded_to_slint_image(thumb.as_ref());
                        models.push(SearchCandidate {
                            song_id: cand.song_id.clone().into(),
                            title: cand.title.clone().into(),
                            artist: cand.artist.clone().into(),
                            album: cand.album.clone().into(),
                            year: cand.year.clone().into(),
                            duration: format_ms_mmss(cand.duration_ms).into(),
                            source: cand.source_name.clone().into(),
                            cover_size: cand.cover_size.clone().into(),
                            cover: cover_img,
                            has_translation: cand.has_translation,
                            is_instrumental: cand.is_instrumental,
                            lyric_preview: cand.lyric_preview.clone().into(),
                        });
                        raw_list.push(cand);
                    }
                    let count = models.len();
                    c.last_combo_candidates = raw_list;
                    c.candidates_model.set_vec(models);
                    ui.global::<AppState>()
                        .set_selected_candidate_index(if count > 0 { 0 } else { -1 });

                    let note = if count > 0 {
                        format!("找到 {count} 条匹配候选")
                    } else if let Some(err) = last_err {
                        format!("搜索未找到结果 ({err})")
                    } else {
                        "未找到匹配的歌曲候选".to_string()
                    };
                    c.sync_counts_and_status(&ui, Some(&note));
                });
            });
        });
    }

    fn spawn_cover_search(&mut self, ui: &AppWindow) {
        self.search_gen = self.search_gen.wrapping_add(1);
        let gen_id = self.search_gen;
        let instance_id = self.instance_id;
        let ui_w = ui.as_weak();

        self.last_cover_candidates.clear();
        self.covers_model.set_vec(Vec::new());

        let state = ui.global::<AppState>();
        state.set_selected_cover_index(-1);
        let cfg = read_config_from_ui(ui, self);
        let query = SearchQuery {
            title: state.get_combo_query_title().trim().to_string(),
            artist: state.get_combo_query_artist().trim().to_string(),
            album: state.get_combo_query_album().trim().to_string(),
            song_id: None,
            limit: cfg.search_limit.max(1).min(24),
        };

        let has_query =
            !query.title.is_empty() || !query.artist.is_empty() || !query.album.is_empty();
        if !has_query {
            state.set_is_searching(false);
            self.sync_counts_and_status(ui, Some("请输入标题、艺术家或专辑名称"));
            return;
        }

        let sources = self.source_registry.active_cover_sources();
        if sources.is_empty() {
            state.set_is_searching(false);
            self.sync_counts_and_status(
                ui,
                Some("没有已启用的图片源，请在「选项 -> 标签源」中启用"),
            );
            return;
        }

        state.set_is_searching(true);
        self.sync_counts_and_status(ui, Some("正在搜索专辑封面..."));

        std::thread::spawn(move || {
            let mut items = Vec::new();
            let mut last_err: Option<String> = None;

            for src in &sources {
                match src.search_cover(&query) {
                    Ok(mut list) if !list.is_empty() => {
                        items.append(&mut list);
                        if items.len() >= query.limit {
                            items.truncate(query.limit);
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(err) => {
                        last_err = Some(format!("{}: {err:#}", src.info().name));
                    }
                }
            }

            let source_map: HashMap<String, Arc<dyn TagSource>> = sources
                .iter()
                .map(|s| (s.info().id, Arc::clone(s)))
                .collect();

            let fetched: Vec<(CoverSearchItem, Option<DecodedRgba>)> = std::thread::scope(|s| {
                let handles: Vec<_> = items
                    .into_iter()
                    .take(12)
                    .map(|mut item| {
                        let src_opt = source_map.get(&item.source_id).cloned();
                        s.spawn(move || {
                            let bytes_opt = if let Some(b) = item.thumb_bytes.clone() {
                                Some(b)
                            } else if let Some(ref src) = src_opt {
                                src.download_bytes(&item.url).ok()
                            } else {
                                None
                            };

                            let mut decoded = None;
                            if let Some(bytes) = bytes_opt {
                                if let Ok(cover_data) = build_cover_data_from_bytes(bytes.clone(), "") {
                                    item.width = Some(cover_data.width);
                                    item.height = Some(cover_data.height);
                                    item.size_bytes = Some(cover_data.size_bytes);
                                    if item.format.is_empty() {
                                        item.format = mime_to_label(&cover_data.mime).to_string();
                                    }
                                    decoded = Some(DecodedRgba {
                                        rgba: cover_data.rgba_thumb,
                                        width: cover_data.thumb_width,
                                        height: cover_data.thumb_height,
                                    });
                                    item.thumb_bytes = Some(bytes);
                                }
                            }
                            (item, decoded)
                        })
                    })
                    .collect();

                handles
                    .into_iter()
                    .filter_map(|h| h.join().ok())
                    .collect()
            });

            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = ui_w.upgrade() else { return };
                with_controller(instance_id, |c| {
                    if c.search_gen != gen_id {
                        return;
                    }
                    ui.global::<AppState>().set_is_searching(false);
                    let mut models = Vec::with_capacity(fetched.len());
                    let mut raw_items = Vec::with_capacity(fetched.len());
                    for (item, thumb) in fetched {
                        let res_str = match (item.width, item.height) {
                            (Some(w), Some(h)) if w > 0 && h > 0 => format!("{w}×{h}"),
                            _ => "原图".to_string(),
                        };
                        let size_str = match item.size_bytes {
                            Some(b) if b > 0 => format_size_bytes(b),
                            _ => "未知大小".to_string(),
                        };
                        models.push(CoverCandidate {
                            source: item.source_name.clone().into(),
                            resolution: res_str.into(),
                            size_kb: size_str.into(),
                            format: item.format.clone().into(),
                            title: item.title.clone().into(),
                            cover: decoded_to_slint_image(thumb.as_ref()),
                        });
                        raw_items.push(item);
                    }
                    let count = models.len();
                    c.last_cover_candidates = raw_items;
                    c.covers_model.set_vec(models);
                    ui.global::<AppState>()
                        .set_selected_cover_index(if count > 0 { 0 } else { -1 });

                    let note = if count > 0 {
                        format!("找到 {count} 张封面候选")
                    } else if let Some(err) = last_err {
                        format!("封面搜索未找到结果 ({err})")
                    } else {
                        "未找到匹配的封面候选".to_string()
                    };
                    c.sync_counts_and_status(&ui, Some(&note));
                });
            });
        });
    }

    fn spawn_apply_combo_candidate(
        &mut self,
        ui: &AppWindow,
        cand_idx: i32,
        save_immediately: bool,
    ) {
        if cand_idx < 0 {
            return;
        }
        let Some(cand) = self.last_combo_candidates.get(cand_idx as usize).cloned() else {
            return;
        };
        let Some(m_idx) = self
            .selected_master_index(ui)
            .or_else(|| self.checked_master_indices().into_iter().next())
        else {
            return;
        };

        let target_path = self.disk_tracks[m_idx].path.clone();
        let disk_track = self.disk_tracks[m_idx].clone();
        let working_has_cover = self.working_tracks[m_idx].cover.is_some();
        let state = ui.global::<AppState>();
        let flags = ComboOverwriteFlags::from_state(&state);
        let cfg = read_config_from_ui(ui, self);
        let lyric_opts = cfg.to_lyric_options();
        let write_opts = cfg.to_write_options();
        let source_opt = self.source_registry.get(&cand.source_id);
        let format_reg = Arc::clone(&self.format_registry);
        let instance_id = self.instance_id;
        let ui_w = ui.as_weak();

        state.set_active_dialog(ActiveDialog::None);
        ui.invoke_focus_main();
        self.sync_counts_and_status(ui, Some(&format!("正在获取并应用: {}...", cand.title)));

        std::thread::spawn(move || {
            let mut full_cand = cand.clone();
            if let Some(ref src) = source_opt {
                if !full_cand.song_id.is_empty()
                    && (full_cand.raw_lrc.is_none() || full_cand.track.is_empty())
                {
                    if let Ok(detail) = src.fetch_detail(&full_cand.song_id) {
                        merge_candidate_detail(&mut full_cand, detail);
                    }
                }
                if flags.lyrics && full_cand.raw_lrc.is_none() && src.info().capabilities.lyrics {
                    if let Ok(lrc) = src.fetch_lyrics(&full_cand.song_id) {
                        full_cand.nolyric = lrc.nolyric;
                        full_cand.raw_lrc = lrc.raw_lrc;
                        full_cand.raw_tlyric = lrc.raw_tlyric;
                    }
                }
                if flags.cover
                    && full_cand.cover_bytes.is_none()
                    && (write_opts.overwrite_cover || !working_has_cover)
                {
                    if let Some(ref url) = full_cand.cover_url {
                        if let Ok(bytes) = src.download_bytes(url) {
                            full_cand.cover_bytes = Some(bytes);
                        }
                    }
                }
            }

            let (patch, cover_data_opt, inst_skipped) = build_combo_patch(
                &full_cand,
                &flags,
                &lyric_opts,
                &write_opts,
                working_has_cover,
            );

            let immediate_res = if save_immediately && !patch.is_empty() {
                let write_res = format_reg
                    .write_patch(&target_path, &patch, &write_opts)
                    .and_then(|()| format_reg.read(&target_path));
                Some(write_res)
            } else {
                None
            };

            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = ui_w.upgrade() else { return };
                with_controller(instance_id, |c| {
                    let Some(cur_idx) = c
                        .disk_tracks
                        .iter()
                        .position(|t| t.path == target_path)
                    else {
                        return;
                    };

                    if let Some(res) = immediate_res {
                        match res {
                            Ok(fresh) => {
                                let path_str = target_path.to_string_lossy().into_owned();
                                if c.history_store
                                    .list_for_track(&path_str, 1)
                                    .map(|r| r.is_empty())
                                    .unwrap_or(false)
                                {
                                    let _ = c
                                        .history_store
                                        .record_track(&disk_track, "匹配前原始标签快照");
                                }
                                if cfg.save_lrc_with_tag && !fresh.lyrics.trim().is_empty() {
                                    let _ = export_lrc_for_track(&fresh, &fresh.lyrics, &cfg);
                                }
                                let _ = c.history_store.record_track(
                                    &fresh,
                                    &format!("组合标签匹配并保存 · {}", full_cand.title),
                                );
                                c.disk_tracks[cur_idx] = fresh;
                                c.staged_patches[cur_idx] = TagPatch::default();
                                c.staged_cover_previews[cur_idx] = None;
                                c.extra_modified[cur_idx] = false;
                                c.extra_instrumental[cur_idx] = inst_skipped.then_some(true);
                                c.refresh_track_row(cur_idx);
                                c.reload_history_from_db();
                                c.sync_selected_track_inputs(&ui);
                                let note = if inst_skipped {
                                    format!("已保存标签（纯音乐已跳过歌词）: {}", full_cand.title)
                                } else {
                                    format!("已应用并保存组合标签: {}", full_cand.title)
                                };
                                c.sync_counts_and_status(&ui, Some(&note));
                            }
                            Err(err) => {
                                c.sync_counts_and_status(
                                    &ui,
                                    Some(&format!("保存组合标签失败: {err:#}")),
                                );
                            }
                        }
                    } else {
                        merge_tag_patch(&mut c.staged_patches[cur_idx], &patch);
                        if let Some(cd) = cover_data_opt {
                            c.staged_cover_previews[cur_idx] = Some(cd);
                        }
                        if inst_skipped {
                            c.extra_instrumental[cur_idx] = Some(true);
                        }
                        c.refresh_track_row(cur_idx);
                        c.sync_selected_track_inputs(&ui);
                        let note = if inst_skipped {
                            format!("已暂存组合标签（纯音乐已跳过歌词）: {}", full_cand.title)
                        } else {
                            format!("已应用组合标签（待保存）: {}", full_cand.title)
                        };
                        c.sync_counts_and_status(&ui, Some(&note));
                    }
                });
            });
        });
    }

    fn spawn_apply_lyric_candidate(&mut self, ui: &AppWindow, cand_idx: i32) {
        if cand_idx < 0 {
            return;
        }
        let Some(cand) = self.last_combo_candidates.get(cand_idx as usize).cloned() else {
            return;
        };
        let targets = self.batch_target_master_indices(ui);
        if targets.is_empty() {
            return;
        }
        let target_paths: Vec<PathBuf> = targets
            .iter()
            .map(|&i| self.disk_tracks[i].path.clone())
            .collect();

        let cfg = read_config_from_ui(ui, self);
        let lyric_opts = cfg.to_lyric_options();
        let source_opt = self.source_registry.get(&cand.source_id);
        let instance_id = self.instance_id;
        let ui_w = ui.as_weak();

        ui.global::<AppState>()
            .set_active_dialog(ActiveDialog::None);
        ui.invoke_focus_main();
        self.sync_counts_and_status(ui, Some(&format!("正在获取歌词: {}...", cand.title)));

        std::thread::spawn(move || {
            let mut raw_lrc = cand.raw_lrc.clone();
            let mut raw_tlyric = cand.raw_tlyric.clone();
            let mut nolyric = cand.nolyric;

            if raw_lrc.is_none() && !cand.song_id.is_empty() {
                if let Some(ref src) = source_opt {
                    if src.info().capabilities.lyrics {
                        if let Ok(lrc) = src.fetch_lyrics(&cand.song_id) {
                            raw_lrc = lrc.raw_lrc;
                            raw_tlyric = lrc.raw_tlyric;
                            nolyric = lrc.nolyric;
                        }
                    }
                }
            }

            let processed = process_netease_lyrics(
                raw_lrc.as_deref(),
                raw_tlyric.as_deref(),
                nolyric,
                &cand.title,
                &lyric_opts,
            );

            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = ui_w.upgrade() else { return };
                with_controller(instance_id, |c| {
                    match processed {
                        ProcessedLyrics::Lyrics(text) => {
                            for path in &target_paths {
                                if let Some(idx) = c.disk_tracks.iter().position(|t| &t.path == path) {
                                    c.staged_patches[idx].lyrics = FieldUpdate::Set(text.clone());
                                    c.extra_instrumental[idx] = Some(false);
                                    c.refresh_track_row(idx);
                                }
                            }
                            c.sync_selected_track_inputs(&ui);
                            c.sync_counts_and_status(
                                &ui,
                                Some(&format!("已应用歌词 ({})，请保存生效", cand.source_name)),
                            );
                        }
                        ProcessedLyrics::InstrumentalSkipped => {
                            for path in &target_paths {
                                if let Some(idx) = c.disk_tracks.iter().position(|t| &t.path == path) {
                                    c.extra_instrumental[idx] = Some(true);
                                    c.refresh_track_row(idx);
                                }
                            }
                            c.sync_selected_track_inputs(&ui);
                            c.sync_counts_and_status(
                                &ui,
                                Some("检测到纯音乐标记，按规则跳过歌词写入"),
                            );
                        }
                        ProcessedLyrics::Empty => {
                            c.sync_counts_and_status(&ui, Some("所选候选暂无可用歌词"));
                        }
                    }
                });
            });
        });
    }

    fn spawn_apply_cover_candidate(&mut self, ui: &AppWindow, cov_idx: i32) {
        if cov_idx < 0 {
            return;
        }
        let Some(cov) = self.last_cover_candidates.get(cov_idx as usize).cloned() else {
            return;
        };
        let targets = self.batch_target_master_indices(ui);
        if targets.is_empty() {
            return;
        }
        let cfg = read_config_from_ui(ui, self);
        let overwrite = cfg.overwrite_cover;
        let target_paths: Vec<PathBuf> = targets
            .into_iter()
            .filter(|&i| overwrite || self.working_tracks[i].cover.is_none())
            .map(|i| self.disk_tracks[i].path.clone())
            .collect();

        if target_paths.is_empty() {
            ui.global::<AppState>()
                .set_active_dialog(ActiveDialog::None);
            ui.invoke_focus_main();
            self.sync_counts_and_status(ui, Some("已有封面且未勾选覆盖，已跳过"));
            return;
        }

        let source_opt = self.source_registry.get(&cov.source_id);
        let write_opts = cfg.to_write_options();
        let instance_id = self.instance_id;
        let ui_w = ui.as_weak();

        ui.global::<AppState>()
            .set_active_dialog(ActiveDialog::None);
        ui.invoke_focus_main();
        self.sync_counts_and_status(ui, Some("正在下载高清专辑封面..."));

        std::thread::spawn(move || {
            let bytes_res: Result<Vec<u8>> = if !cov.url.is_empty() {
                if let Some(ref src) = source_opt {
                    src.download_bytes(&cov.url)
                        .or_else(|e| cov.thumb_bytes.clone().ok_or(e))
                } else if let Some(b) = cov.thumb_bytes.clone() {
                    Ok(b)
                } else {
                    Err(anyhow::anyhow!("cover source unavailable"))
                }
            } else if let Some(b) = cov.thumb_bytes.clone() {
                Ok(b)
            } else {
                Err(anyhow::anyhow!("cover URL is empty"))
            };

            let result = bytes_res.and_then(|raw_bytes| {
                let spec = CoverWriteSpec {
                    bytes: raw_bytes,
                    mime: String::new(),
                    target_width: None,
                    target_height: None,
                    keep_aspect: true,
                    max_kb: (write_opts.pic_size_limit_kb > 0)
                        .then_some(write_opts.pic_size_limit_kb),
                    format: write_opts.pic_format,
                    overwrite: true,
                };
                let (final_bytes, mime) = process_cover_image(
                    &spec,
                    write_opts.pic_res_limit,
                    write_opts.pic_size_limit_kb,
                    write_opts.pic_format,
                )?;
                let mime_str = mime.as_str().to_string();
                let cover_data = build_cover_data_from_bytes(final_bytes.clone(), &mime_str)?;
                let final_spec = CoverWriteSpec {
                    bytes: final_bytes,
                    mime: mime_str,
                    target_width: None,
                    target_height: None,
                    keep_aspect: true,
                    max_kb: None,
                    format: PicFormatLimit::Auto,
                    overwrite: true,
                };
                Ok((final_spec, cover_data))
            });

            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = ui_w.upgrade() else { return };
                with_controller(instance_id, |c| match result {
                    Ok((spec, cover_data)) => {
                        let mut applied = 0usize;
                        for path in &target_paths {
                            if let Some(idx) = c.disk_tracks.iter().position(|t| &t.path == path) {
                                c.staged_patches[idx].cover = FieldUpdate::Set(spec.clone());
                                c.staged_cover_previews[idx] = Some(cover_data.clone());
                                c.refresh_track_row(idx);
                                applied += 1;
                            }
                        }
                        c.sync_selected_track_inputs(&ui);
                        c.sync_counts_and_status(
                            &ui,
                            Some(&format!("已应用封面到 {applied} 首曲目 ({})", cov.source_name)),
                        );
                    }
                    Err(err) => {
                        c.sync_counts_and_status(&ui, Some(&format!("封面下载失败: {err:#}")));
                    }
                });
            });
        });
    }

    fn apply_cover_resolution(&mut self, ui: &AppWindow) {
        let targets = self.batch_target_master_indices(ui);
        if targets.is_empty() {
            return;
        }
        let state = ui.global::<AppState>();
        let tw = state
            .get_cover_target_width()
            .trim()
            .parse::<u32>()
            .ok()
            .filter(|&v| v > 0);
        let th = state
            .get_cover_target_height()
            .trim()
            .parse::<u32>()
            .ok()
            .filter(|&v| v > 0);
        let max_kb = state
            .get_cover_target_max_kb()
            .trim()
            .parse::<u32>()
            .ok()
            .filter(|&v| v > 0);
        let keep_aspect = state.get_cover_keep_aspect();
        let format = PicFormatLimit::from_index(state.get_cover_format_index());

        let mut updated = 0usize;
        let mut last_err = None;

        for m_idx in targets {
            let Some(existing_cover) = self.working_tracks[m_idx].cover.clone() else {
                continue;
            };
            let spec = CoverWriteSpec {
                bytes: existing_cover.bytes,
                mime: existing_cover.mime,
                target_width: tw,
                target_height: th,
                keep_aspect,
                max_kb,
                format,
                overwrite: true,
            };
            match process_cover_image(&spec, 0, max_kb.unwrap_or(0), format)
                .and_then(|(bytes, mime)| {
                    let mime_str = mime.as_str().to_string();
                    let data = build_cover_data_from_bytes(bytes.clone(), &mime_str)?;
                    let staged = CoverWriteSpec {
                        bytes,
                        mime: mime_str,
                        target_width: None,
                        target_height: None,
                        keep_aspect,
                        max_kb: None,
                        format: PicFormatLimit::Auto,
                        overwrite: true,
                    };
                    Ok((staged, data))
                }) {
                Ok((staged, data)) => {
                    self.staged_patches[m_idx].cover = FieldUpdate::Set(staged);
                    self.staged_cover_previews[m_idx] = Some(data);
                    self.refresh_track_row(m_idx);
                    updated += 1;
                }
                Err(err) => {
                    last_err = Some(err);
                }
            }
        }

        state.set_active_dialog(ActiveDialog::None);
        self.sync_selected_track_inputs(ui);
        if let Some(err) = last_err {
            self.sync_counts_and_status(&ui, Some(&format!("部分封面处理失败: {err:#}")));
        } else {
            self.sync_counts_and_status(&ui, Some(&format!("已调整 {updated} 首曲目封面分辨率")));
        }
    }

    fn extract_cover(&mut self, ui: &AppWindow) {
        let targets = self.batch_target_master_indices(ui);
        if targets.is_empty() {
            return;
        }
        let mut extracted = 0usize;
        let mut last_path = None;
        let mut last_err = None;

        for m_idx in targets {
            if let Some(ref cover) = self.staged_cover_previews[m_idx] {
                let ext = mime_to_ext(&cover.mime);
                let out_path = self.working_tracks[m_idx].path.with_extension(ext);
                match std::fs::write(&out_path, &cover.bytes) {
                    Ok(()) => {
                        extracted += 1;
                        last_path = Some(out_path);
                    }
                    Err(e) => last_err = Some(anyhow::Error::from(e)),
                }
            } else {
                match extract_cover_to_sidecar(&self.disk_tracks[m_idx].path) {
                    Ok(p) => {
                        extracted += 1;
                        last_path = Some(p);
                    }
                    Err(e) => last_err = Some(e),
                }
            }
        }

        if extracted > 0 {
            let suffix = last_path
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                .unwrap_or_default();
            self.sync_counts_and_status(
                ui,
                Some(&format!("已提取 {extracted} 张封面到歌曲目录 ({suffix})")),
            );
        } else if let Some(err) = last_err {
            self.sync_counts_and_status(ui, Some(&format!("提取封面失败: {err:#}")));
        }
    }

    fn choose_local_cover(&mut self, ui: &AppWindow) {
        let targets = self.batch_target_master_indices(ui);
        if targets.is_empty() {
            return;
        }
        let Some(img_path) = rfd::FileDialog::new()
            .set_title("选择本地封面图片")
            .add_filter("图片文件", &["jpg", "jpeg", "png", "webp", "bmp", "gif"])
            .pick_file()
        else {
            return;
        };

        let bytes = match std::fs::read(&img_path) {
            Ok(b) => b,
            Err(err) => {
                self.sync_counts_and_status(ui, Some(&format!("读取封面文件失败: {err}")));
                return;
            }
        };

        let cfg = read_config_from_ui(ui, self);
        let write_opts = cfg.to_write_options();
        let spec = CoverWriteSpec {
            bytes,
            mime: String::new(),
            target_width: None,
            target_height: None,
            keep_aspect: true,
            max_kb: (write_opts.pic_size_limit_kb > 0).then_some(write_opts.pic_size_limit_kb),
            format: write_opts.pic_format,
            overwrite: true,
        };

        match process_cover_image(
            &spec,
            write_opts.pic_res_limit,
            write_opts.pic_size_limit_kb,
            write_opts.pic_format,
        )
        .and_then(|(b, mime)| {
            let mime_str = mime.as_str().to_string();
            let cover_data = build_cover_data_from_bytes(b.clone(), &mime_str)?;
            let staged = CoverWriteSpec {
                bytes: b,
                mime: mime_str,
                target_width: None,
                target_height: None,
                keep_aspect: true,
                max_kb: None,
                format: PicFormatLimit::Auto,
                overwrite: true,
            };
            Ok((staged, cover_data))
        }) {
            Ok((staged, cover_data)) => {
                for m_idx in targets {
                    self.staged_patches[m_idx].cover = FieldUpdate::Set(staged.clone());
                    self.staged_cover_previews[m_idx] = Some(cover_data.clone());
                    self.refresh_track_row(m_idx);
                }
                self.sync_selected_track_inputs(ui);
                self.sync_counts_and_status(ui, Some("已载入本地封面图片（请保存生效）"));
            }
            Err(err) => {
                self.sync_counts_and_status(ui, Some(&format!("处理本地封面失败: {err:#}")));
            }
        }
    }

    fn spawn_auto_match(&mut self, ui: &AppWindow) {
        let targets = self.batch_target_master_indices(ui);
        if targets.is_empty() {
            self.sync_counts_and_status(ui, Some("请先选择需要自动匹配的曲目"));
            return;
        }

        if let Some(prev_cancel) = self.active_am_cancel.take() {
            prev_cancel.store(true, Ordering::SeqCst);
        }

        let sources = self.source_registry.active_combo_sources();
        if sources.is_empty() {
            self.sync_counts_and_status(
                ui,
                Some("没有已启用的标签源，请在「选项 -> 标签源」中启用"),
            );
            return;
        }

        let cancel_flag = Arc::new(AtomicBool::new(false));
        self.active_am_cancel = Some(Arc::clone(&cancel_flag));

        let state = ui.global::<AppState>();
        let cfg = read_config_from_ui(ui, self);
        let mut flags = ComboOverwriteFlags::from_state(&state);
        if flags.write_163_key {
            flags.comment = true;
        }
        let lyric_opts = cfg.to_lyric_options();
        let write_opts = cfg.to_write_options();
        let thread_count = cfg.thread_count.clamp(1, 16);

        let jobs: Vec<(PathBuf, AudioTrack)> = targets
            .into_iter()
            .map(|i| (self.disk_tracks[i].path.clone(), self.working_tracks[i].clone()))
            .collect();
        let total = jobs.len();

        self.auto_match_model.set_vec(Vec::new());
        state.set_progress_title("正在批量自动匹配标签...".into());
        state.set_progress_file("".into());
        state.set_progress_value(0.0);
        state.set_progress_ok(0);
        state.set_progress_fail(0);
        state.set_progress_skip(0);
        state.set_progress_total(total as i32);
        state.set_active_dialog(ActiveDialog::AutoMatch);
        self.sync_counts_and_status(ui, Some(&format!("开始自动匹配 ({total} 首)...")));

        let instance_id = self.instance_id;
        let ui_w = ui.as_weak();

        std::thread::spawn(move || {
            let queue = Mutex::new(VecDeque::from(jobs));
            let ok_cnt = AtomicUsize::new(0);
            let fail_cnt = AtomicUsize::new(0);
            let skip_cnt = AtomicUsize::new(0);
            let done_cnt = AtomicUsize::new(0);

            std::thread::scope(|s| {
                let workers = thread_count.min(total).max(1);
                for _ in 0..workers {
                    s.spawn(|| {
                        while !cancel_flag.load(Ordering::Relaxed) {
                            let job = { queue.lock().ok().and_then(|mut q| q.pop_front()) };
                            let Some((path, track)) = job else {
                                break;
                            };

                            let outcome = auto_match_single_track(
                                &track,
                                &sources,
                                &cfg,
                                &flags,
                                &lyric_opts,
                                &write_opts,
                            );

                            let (row, patch_opt, cover_opt, inst_flag) = match outcome {
                                AutoMatchTrackOutcome::Matched {
                                    cand,
                                    patch,
                                    cover_data,
                                    instrumental_skipped,
                                } => {
                                    if instrumental_skipped {
                                        skip_cnt.fetch_add(1, Ordering::SeqCst);
                                    } else {
                                        ok_cnt.fetch_add(1, Ordering::SeqCst);
                                    }
                                    let status = if instrumental_skipped {
                                        "已匹配 (纯音乐)"
                                    } else {
                                        "已匹配"
                                    };
                                    let detail = format!(
                                        "{} #{} · {}",
                                        cand.source_name,
                                        cand.song_id,
                                        if instrumental_skipped {
                                            "跳过歌词写入"
                                        } else if cand.has_translation {
                                            "双语歌词"
                                        } else {
                                            "标签已匹配"
                                        }
                                    );
                                    let row = AutoMatchRow {
                                        filename: track.filename.clone().into(),
                                        matched_title: cand.title.into(),
                                        matched_artist: cand.artist.into(),
                                        status: status.into(),
                                        detail: detail.into(),
                                    };
                                    (row, Some(patch), cover_data, instrumental_skipped)
                                }
                                AutoMatchTrackOutcome::NotFound(reason) => {
                                    fail_cnt.fetch_add(1, Ordering::SeqCst);
                                    let row = AutoMatchRow {
                                        filename: track.filename.clone().into(),
                                        matched_title: "".into(),
                                        matched_artist: "".into(),
                                        status: "未找到".into(),
                                        detail: reason.into(),
                                    };
                                    (row, None, None, false)
                                }
                            };

                            let done = done_cnt.fetch_add(1, Ordering::SeqCst) + 1;
                            let ok_now = ok_cnt.load(Ordering::SeqCst) as i32;
                            let fail_now = fail_cnt.load(Ordering::SeqCst) as i32;
                            let skip_now = skip_cnt.load(Ordering::SeqCst) as i32;
                            let filename_now = track.filename.clone();
                            let ui_w2 = ui_w.clone();
                            let path_clone = path.clone();

                            let _ = slint::invoke_from_event_loop(move || {
                                let Some(ui) = ui_w2.upgrade() else { return };
                                with_controller(instance_id, |c| {
                                    c.auto_match_model.push(row);
                                    if let Some(patch) = patch_opt {
                                        if let Some(idx) = c
                                            .disk_tracks
                                            .iter()
                                            .position(|t| t.path == path_clone)
                                        {
                                            merge_tag_patch(&mut c.staged_patches[idx], &patch);
                                            if let Some(cd) = cover_opt {
                                                c.staged_cover_previews[idx] = Some(cd);
                                            }
                                            if inst_flag {
                                                c.extra_instrumental[idx] = Some(true);
                                            }
                                            c.refresh_track_row(idx);
                                        }
                                    }
                                    let st = ui.global::<AppState>();
                                    st.set_progress_file(filename_now.into());
                                    st.set_progress_ok(ok_now);
                                    st.set_progress_fail(fail_now);
                                    st.set_progress_skip(skip_now);
                                    st.set_progress_value(done as f32 / total as f32);
                                });
                            });
                        }
                    });
                }
            });

            let was_cancelled = cancel_flag.load(Ordering::SeqCst);
            let ok_final = ok_cnt.load(Ordering::SeqCst);
            let fail_final = fail_cnt.load(Ordering::SeqCst);
            let skip_final = skip_cnt.load(Ordering::SeqCst);

            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = ui_w.upgrade() else { return };
                with_controller(instance_id, |c| {
                    c.active_am_cancel = None;
                    c.sync_selected_track_inputs(&ui);
                    let st = ui.global::<AppState>();
                    st.set_progress_value(1.0);
                    let msg = if was_cancelled {
                        format!("自动匹配已取消（已匹配 {ok_final} 首，跳过 {skip_final} 首）")
                    } else {
                        format!(
                            "自动匹配完成 · 成功 {ok_final} / 纯音乐跳过 {skip_final} / 未找到 {fail_final}（请保存生效）"
                        )
                    };
                    st.set_progress_title(msg.clone().into());
                    c.sync_counts_and_status(&ui, Some(&msg));
                });
            });
        });
    }

    fn spawn_replaygain_scan(&mut self, ui: &AppWindow, mode_override: Option<RgScanMode>) {
        let targets = self.batch_target_master_indices(ui);
        if targets.is_empty() {
            self.sync_counts_and_status(ui, Some("请先选择需要扫描 ReplayGain 的曲目"));
            return;
        }

        if let Some(prev_cancel) = self.active_rg_cancel.take() {
            prev_cancel.store(true, Ordering::SeqCst);
        }

        let cfg = read_config_from_ui(ui, self);
        let mode = mode_override.unwrap_or_else(|| cfg.to_rg_scan_mode());
        let target_lufs = cfg.rg_target_lufs;
        let clip_prevent = cfg.rg_clip_prevent;
        let write_opts = cfg.to_write_options();

        let paths: Vec<PathBuf> = targets
            .iter()
            .map(|&i| self.disk_tracks[i].path.clone())
            .collect();
        let total = paths.len();
        let first_name = self.disk_tracks[targets[0]].filename.clone();

        let cancel_flag = Arc::new(AtomicBool::new(false));
        self.active_rg_cancel = Some(Arc::clone(&cancel_flag));

        let state = ui.global::<AppState>();
        state.set_progress_title("正在计算并写入 ReplayGain 回放增益...".into());
        state.set_progress_file(first_name.into());
        state.set_progress_value(0.0);
        state.set_progress_ok(0);
        state.set_progress_fail(0);
        state.set_progress_skip(0);
        state.set_progress_total(total as i32);
        state.set_active_dialog(ActiveDialog::Progress);

        let format_reg = Arc::clone(&self.format_registry);
        let instance_id = self.instance_id;
        let ui_w = ui.as_weak();

        std::thread::spawn(move || {
            let scanner = EbuR128Scanner::with_cancel_flag(Arc::clone(&cancel_flag));
            let progress_ui = ui_w.clone();
            let progress_cb = move |done: usize, tot: usize, path: &Path| {
                let fname = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let ui_w3 = progress_ui.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = ui_w3.upgrade() else { return };
                    let st = ui.global::<AppState>();
                    st.set_progress_file(fname.into());
                    st.set_progress_value((done as f32 / tot.max(1) as f32) * 0.85);
                });
            };

            let mut scanned_ok: Vec<(PathBuf, ReplayGainData)> = Vec::new();
            let mut fail_count = 0usize;

            match scanner.scan_tracks_with_progress(
                &paths,
                mode,
                target_lufs,
                clip_prevent,
                Some(&progress_cb),
            ) {
                Ok(results) => {
                    scanned_ok = results;
                }
                Err(_) if cancel_flag.load(Ordering::SeqCst) => {}
                Err(_) => {
                    // Fallback per directory (AlbumAndTrack) or per file (TrackOnly) so one unsupported file
                    // doesn't abort valid siblings.
                    if mode == RgScanMode::AlbumAndTrack {
                        let mut by_dir: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
                        for p in &paths {
                            by_dir
                                .entry(p.parent().map(Path::to_path_buf).unwrap_or_default())
                                .or_default()
                                .push(p.clone());
                        }
                        let mut done_so_far = 0usize;
                        for group in by_dir.into_values() {
                            if cancel_flag.load(Ordering::SeqCst) {
                                break;
                            }
                            match scanner.scan_tracks(&group, mode, target_lufs, clip_prevent) {
                                Ok(mut res) => scanned_ok.append(&mut res),
                                Err(_) => {
                                    for p in &group {
                                        if cancel_flag.load(Ordering::SeqCst) {
                                            break;
                                        }
                                        match scanner.scan_tracks(
                                            std::slice::from_ref(p),
                                            RgScanMode::TrackOnly,
                                            target_lufs,
                                            clip_prevent,
                                        ) {
                                            Ok(mut r) => scanned_ok.append(&mut r),
                                            Err(_) => fail_count += 1,
                                        }
                                    }
                                }
                            }
                            done_so_far += group.len();
                            progress_cb(
                                done_so_far,
                                total,
                                group.last().map(PathBuf::as_path).unwrap_or(Path::new("")),
                            );
                        }
                    } else {
                        for (idx, p) in paths.iter().enumerate() {
                            if cancel_flag.load(Ordering::SeqCst) {
                                break;
                            }
                            match scanner.scan_tracks(
                                std::slice::from_ref(p),
                                mode,
                                target_lufs,
                                clip_prevent,
                            ) {
                                Ok(mut res) => scanned_ok.append(&mut res),
                                Err(_) => fail_count += 1,
                            }
                            progress_cb(idx + 1, total, p);
                        }
                    }
                }
            }

            let mut written: Vec<(PathBuf, ReplayGainData)> = Vec::new();
            for (path, rg) in scanned_ok {
                if cancel_flag.load(Ordering::SeqCst) {
                    break;
                }
                match format_reg.write_replaygain(&path, &rg, &write_opts) {
                    Ok(()) => written.push((path, rg)),
                    Err(err) => {
                        eprintln!("ReplayGain write failed for {}: {err:#}", path.display());
                        fail_count += 1;
                    }
                }
            }

            let ok_count = written.len();
            let cancelled = cancel_flag.load(Ordering::SeqCst);

            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = ui_w.upgrade() else { return };
                with_controller(instance_id, |c| {
                    c.active_rg_cancel = None;
                    for (path, rg) in written {
                        if let Some(idx) = c.disk_tracks.iter().position(|t| t.path == path) {
                            c.disk_tracks[idx].replaygain = rg;
                            c.refresh_track_row(idx);
                        }
                    }
                    c.sync_selected_track_inputs(&ui);

                    let st = ui.global::<AppState>();
                    st.set_progress_ok(ok_count as i32);
                    st.set_progress_fail(fail_count as i32);
                    st.set_progress_value(1.0);
                    let title = if cancelled {
                        format!("ReplayGain 扫描已取消（已写入 {ok_count} 首）")
                    } else if fail_count > 0 {
                        format!("ReplayGain 完成 · 成功 {ok_count} 首，失败 {fail_count} 首")
                    } else {
                        format!("ReplayGain 扫描并写入完成（共 {ok_count} 首）")
                    };
                    st.set_progress_title(title.clone().into());
                    c.sync_counts_and_status(&ui, Some(&title));
                });
            });
        });
    }

    fn handle_lyric_action(&mut self, ui: &AppWindow, action: &str) {
        let state = ui.global::<AppState>();
        let active_dlg = state.get_active_dialog();
        let text = state.get_lyric_editor_text().to_string();

        match action {
            "delete-head-tag" => {
                let cleaned = delete_head_tags(&text);
                state.set_lyric_editor_text(cleaned.into());
                self.sync_counts_and_status(ui, Some("已删除歌词头部标签"));
            }
            "delete-blank-lines" => {
                let cleaned = delete_blank_lines(&text);
                state.set_lyric_editor_text(cleaned.into());
                self.sync_counts_and_status(ui, Some("已删除空白文字行"));
            }
            "reformat-timetag" => {
                let cleaned = reformat_timetags(&text);
                state.set_lyric_editor_text(cleaned.into());
                self.sync_counts_and_status(ui, Some("已格式化时间轴 (2位毫秒)"));
            }
            "remove-timetag" => {
                let cleaned = remove_timetags(&text);
                state.set_lyric_editor_text(cleaned.into());
                self.sync_counts_and_status(ui, Some("已移除所有时间轴"));
            }
            "shift-timetag" => {
                let offset = state
                    .get_adjust_offset_ms()
                    .trim()
                    .parse::<f64>()
                    .map(|v| v.round() as i64)
                    .unwrap_or(0);
                let shifted = shift_timetags(&text, offset);
                state.set_lyric_editor_text(shifted.clone().into());
                if self.adjust_from_editor {
                    self.adjust_from_editor = false;
                    state.set_active_dialog(ActiveDialog::LyricEditor);
                    self.sync_counts_and_status(ui, Some(&format!("时间轴已平移 {offset} ms")));
                } else {
                    for m_idx in self.edit_target_master_indices(ui) {
                        self.staged_patches[m_idx].lyrics = FieldUpdate::Set(shifted.clone());
                        self.extra_instrumental[m_idx] = Some(false);
                        self.refresh_track_row(m_idx);
                    }
                    state.set_active_dialog(ActiveDialog::None);
                    self.sync_selected_track_inputs(ui);
                    self.sync_counts_and_status(
                        ui,
                        Some(&format!("时间轴已平移 {offset} ms 并应用")),
                    );
                }
            }
            "reset" => {
                if let Some(m_idx) = self.selected_master_index(ui) {
                    state.set_lyric_editor_text(self.working_tracks[m_idx].lyrics.clone().into());
                }
                self.sync_counts_and_status(ui, Some("已重置歌词编辑器内容"));
            }
            "apply" => {
                let new_lyric = state.get_lyric_editor_text().to_string();
                let targets = self.edit_target_master_indices(ui);
                for m_idx in targets {
                    let disk_lyric = &self.disk_tracks[m_idx].lyrics;
                    self.staged_patches[m_idx].lyrics =
                        if self.edit_target_master_indices(ui).len() == 1 && &new_lyric == disk_lyric
                        {
                            FieldUpdate::Keep
                        } else {
                            FieldUpdate::Set(new_lyric.clone())
                        };
                    self.extra_instrumental[m_idx] = None;
                    self.refresh_track_row(m_idx);
                }
                state.set_active_dialog(ActiveDialog::None);
                self.sync_selected_track_inputs(ui);
                self.sync_counts_and_status(ui, Some("已应用歌词修改（请保存生效）"));
            }
            "import-lrc" => {
                let mut dlg = rfd::FileDialog::new()
                    .set_title("导入 LRC 歌词文件")
                    .add_filter("LRC 歌词", &["lrc", "txt"]);
                if let Some(m_idx) = self.selected_master_index(ui) {
                    let dir = Path::new(&self.working_tracks[m_idx].filedir);
                    if dir.is_dir() {
                        dlg = dlg.set_directory(dir);
                    }
                }
                let Some(lrc_path) = dlg.pick_file() else {
                    return;
                };
                match std::fs::read(&lrc_path) {
                    Ok(bytes) => {
                        let enc = state.get_opt_lrc_encoding().to_string();
                        let content = decode_lrc_bytes(&bytes, &enc);
                        state.set_lyric_editor_text(content.clone().into());
                        if active_dlg != ActiveDialog::LyricEditor {
                            for m_idx in self.edit_target_master_indices(ui) {
                                self.staged_patches[m_idx].lyrics =
                                    FieldUpdate::Set(content.clone());
                                self.extra_instrumental[m_idx] = Some(false);
                                self.refresh_track_row(m_idx);
                            }
                            self.sync_selected_track_inputs(ui);
                        }
                        self.sync_counts_and_status(
                            ui,
                            Some(&format!("已导入歌词: {}", lrc_path.display())),
                        );
                    }
                    Err(err) => {
                        self.sync_counts_and_status(ui, Some(&format!("读取 LRC 失败: {err}")));
                    }
                }
            }
            "save-lrc" => {
                let content = if active_dlg == ActiveDialog::LyricEditor {
                    text
                } else if let Some(m_idx) = self.selected_master_index(ui) {
                    self.working_tracks[m_idx].lyrics.clone()
                } else {
                    String::new()
                };
                if content.trim().is_empty() {
                    self.sync_counts_and_status(ui, Some("当前曲目没有可导出的歌词"));
                    return;
                }
                let cfg = read_config_from_ui(ui, self);
                let mut dlg = rfd::FileDialog::new()
                    .set_title("另存为 LRC 歌词文件")
                    .add_filter("LRC 歌词", &["lrc"]);
                if let Some(m_idx) = self.selected_master_index(ui) {
                    let target = resolve_lrc_target_path(&self.working_tracks[m_idx], &cfg);
                    if let Some(parent) = target.parent() {
                        if parent.is_dir() {
                            dlg = dlg.set_directory(parent);
                        }
                    }
                    if let Some(fname) = target.file_name().and_then(|n| n.to_str()) {
                        dlg = dlg.set_file_name(fname);
                    }
                }
                let Some(out_path) = dlg.save_file() else {
                    return;
                };
                let bytes = encode_lrc_bytes(&content, &cfg.lrc_encoding);
                match std::fs::write(&out_path, bytes) {
                    Ok(()) => {
                        self.sync_counts_and_status(
                            ui,
                            Some(&format!("已导出歌词到 {}", out_path.display())),
                        );
                    }
                    Err(err) => {
                        self.sync_counts_and_status(ui, Some(&format!("保存 LRC 失败: {err}")));
                    }
                }
            }
            _ => {}
        }
    }

    fn handle_find_replace(&mut self, ui: &AppWindow, action: &str) {
        let state = ui.global::<AppState>();
        let find = state.get_find_text().to_string();
        let replace = state.get_replace_text().to_string();
        let match_case = state.get_find_match_case();

        if find.is_empty() {
            self.sync_counts_and_status(ui, Some("请输入要查找的内容"));
            return;
        }

        if self.find_replace_from_editor {
            let cur = state.get_lyric_editor_text().to_string();
            let count = count_occurrences(&cur, &find, match_case);
            match action {
                "find-next" | "find-prev" => {
                    self.sync_counts_and_status(
                        ui,
                        Some(&format!("歌词编辑区共找到 {count} 处匹配")),
                    );
                }
                "replace-one" => {
                    let replaced = replace_first_str(&cur, &find, &replace, match_case);
                    let changed = replaced != cur;
                    state.set_lyric_editor_text(replaced.into());
                    self.sync_counts_and_status(
                        ui,
                        Some(if changed { "已替换歌词中 1 处匹配" } else { "未找到匹配文本" }),
                    );
                }
                "find-replace" => {
                    let replaced = replace_str(&cur, &find, &replace, match_case);
                    state.set_lyric_editor_text(replaced.into());
                    self.find_replace_from_editor = false;
                    state.set_active_dialog(ActiveDialog::LyricEditor);
                    self.sync_counts_and_status(
                        ui,
                        Some(&format!("已在歌词编辑器中替换 {count} 处")),
                    );
                }
                _ => {}
            }
            return;
        }

        match action {
            "find-next" | "find-prev" => {
                if self.filtered_indices.is_empty() {
                    self.sync_counts_and_status(ui, Some("列表为空，未找到匹配项"));
                    return;
                }
                let forward = action == "find-next";
                let len = self.filtered_indices.len();
                let start = state.get_selected_index().max(0) as usize;
                let mut found_v = None;
                for step in 1..=len {
                    let v_idx = if forward {
                        (start + step) % len
                    } else {
                        (start + len - (step % len)) % len
                    };
                    let m_idx = self.filtered_indices[v_idx];
                    if track_matches_query(&self.working_tracks[m_idx], &find, match_case) {
                        found_v = Some(v_idx);
                        break;
                    }
                }
                if let Some(v_idx) = found_v {
                    self.find_cursor = v_idx;
                    state.set_selected_index(v_idx as i32);
                    self.sync_selected_track_inputs(ui);
                    let fname = &self.working_tracks[self.filtered_indices[v_idx]].filename;
                    self.sync_counts_and_status(
                        ui,
                        Some(&format!("定位到第 {} 首匹配: {fname}", v_idx + 1)),
                    );
                } else {
                    self.sync_counts_and_status(ui, Some("未找到匹配的曲目标签"));
                }
            }
            "replace-one" => {
                let Some(m_idx) = self.selected_master_index(ui) else {
                    return;
                };
                let changed =
                    replace_in_track(&mut self.staged_patches[m_idx], &self.working_tracks[m_idx], &find, &replace, match_case);
                if changed {
                    self.refresh_track_row(m_idx);
                    self.sync_selected_track_inputs(ui);
                    self.sync_counts_and_status(ui, Some("已替换当前曲目匹配字段（请保存生效）"));
                } else {
                    self.sync_counts_and_status(ui, Some("当前曲目无可替换的匹配字段"));
                }
            }
            "find-replace" => {
                let checked = self.checked_master_indices();
                let scope = if !checked.is_empty() {
                    checked
                } else {
                    self.filtered_indices.clone()
                };
                let mut changed_tracks = 0usize;
                for m_idx in scope {
                    let current_snapshot = self.working_tracks[m_idx].clone();
                    if replace_in_track(
                        &mut self.staged_patches[m_idx],
                        &current_snapshot,
                        &find,
                        &replace,
                        match_case,
                    ) {
                        self.refresh_track_row(m_idx);
                        changed_tracks += 1;
                    }
                }
                state.set_active_dialog(ActiveDialog::None);
                self.sync_selected_track_inputs(ui);
                self.sync_counts_and_status(
                    ui,
                    Some(&format!("全部替换完成（影响 {changed_tracks} 首曲目）")),
                );
            }
            _ => {}
        }
    }

    fn filename_targets(&self, ui: &AppWindow) -> Vec<usize> {
        let checked = self.checked_master_indices();
        if !checked.is_empty() {
            checked
        } else if !self.filtered_indices.is_empty() {
            let sel = self.selected_master_index(ui);
            if let Some(m) = sel {
                if self.filtered_indices.len() == 1 {
                    vec![m]
                } else {
                    self.filtered_indices.clone()
                }
            } else {
                self.filtered_indices.clone()
            }
        } else {
            Vec::new()
        }
    }

    fn current_regex_mappings(&self) -> Vec<(usize, TemplateField)> {
        (0..self.regex_groups_model.row_count())
            .filter_map(|i| self.regex_groups_model.row_data(i))
            .filter_map(|g| {
                let token = g
                    .target_field
                    .split(" - ")
                    .next()
                    .unwrap_or(g.target_field.as_str())
                    .trim();
                TemplateField::from_token(token).map(|field| (g.group_index.max(1) as usize, field))
            })
            .collect()
    }

    fn refresh_filename_previews_if_changed(&mut self, ui: &AppWindow, force: bool) {
        let state = ui.global::<AppState>();
        let mode = state.get_filename_mode();
        let tab = state.get_filename_rel_tab_index();
        let pat_idx = state.get_filename_pattern_index();
        let custom_pat = state.get_filename_custom_pattern().to_string();
        let regex_input = state.get_filename_regex_input().to_string();
        let rename_extra = state.get_filename_rename_extra();
        let targets = self.filename_targets(ui);

        let sig = format!(
            "{mode}|{tab}|{pat_idx}|{custom_pat}|{regex_input}|{rename_extra}|{}",
            targets
                .iter()
                .map(|&i| format!("{i}:{}", self.working_tracks[i].filename))
                .collect::<Vec<_>>()
                .join(",")
        );

        if !force && self.last_fn_preview_sig.as_deref() == Some(&sig) {
            return;
        }
        self.last_fn_preview_sig = Some(sig);

        let template = resolve_template(pat_idx.max(0) as usize, &custom_pat);
        let regex_mappings = self.current_regex_mappings();
        let mut previews = Vec::with_capacity(targets.len());

        if mode == 1 {
            let subset: Vec<AudioTrack> = targets
                .iter()
                .map(|&i| self.working_tracks[i].clone())
                .collect();
            let plan = plan_batch_renames(&subset, template, rename_extra);
            for (local_i, item) in plan.into_iter().enumerate() {
                let status = match item.status {
                    RenameStatus::Ready => "就绪".to_string(),
                    RenameStatus::Unchanged => "无需重命名".to_string(),
                    RenameStatus::MissingField(msg) => format!("跳过 (缺少{msg})"),
                    RenameStatus::Conflict(msg) => format!("冲突 ({msg})"),
                };
                previews.push(FilenamePreviewItem {
                    before: subset[local_i].filename.clone().into(),
                    after: item.new_filename.into(),
                    status: status.into(),
                });
            }
        } else {
            for &m_idx in &targets {
                let track = &self.working_tracks[m_idx];
                let res = if tab == 1 {
                    filename_to_tags_regex(&track.filename, &regex_input, &regex_mappings)
                } else {
                    filename_to_tags(&track.filename, template)
                };
                match res {
                    Ok(patch) if !patch.is_empty() => {
                        previews.push(FilenamePreviewItem {
                            before: track.filename.clone().into(),
                            after: format_patch_summary(&patch).into(),
                            status: "就绪".into(),
                        });
                    }
                    Ok(_) => {
                        previews.push(FilenamePreviewItem {
                            before: track.filename.clone().into(),
                            after: "无提取字段".into(),
                            status: "跳过".into(),
                        });
                    }
                    Err(_) => {
                        previews.push(FilenamePreviewItem {
                            before: track.filename.clone().into(),
                            after: "无法匹配模板/正则".into(),
                            status: "未匹配".into(),
                        });
                    }
                }
            }
        }

        self.filename_previews_model.set_vec(previews);
    }

    fn apply_filename(&mut self, ui: &AppWindow) {
        let state = ui.global::<AppState>();
        let mode = state.get_filename_mode();
        let tab = state.get_filename_rel_tab_index();
        let pat_idx = state.get_filename_pattern_index();
        let custom_pat = state.get_filename_custom_pattern().to_string();
        let regex_input = state.get_filename_regex_input().to_string();
        let rename_extra = state.get_filename_rename_extra();

        let targets = self.filename_targets(ui);
        if targets.is_empty() {
            state.set_active_dialog(ActiveDialog::None);
            return;
        }

        let template = resolve_template(pat_idx.max(0) as usize, &custom_pat);
        let regex_mappings = self.current_regex_mappings();

        if mode == 0 {
            let mut matched = 0usize;
            for m_idx in targets {
                let res = if tab == 1 {
                    filename_to_tags_regex(
                        &self.working_tracks[m_idx].filename,
                        &regex_input,
                        &regex_mappings,
                    )
                } else {
                    filename_to_tags(&self.working_tracks[m_idx].filename, template)
                };
                if let Ok(patch) = res {
                    if !patch.is_empty() {
                        merge_tag_patch(&mut self.staged_patches[m_idx], &patch);
                        self.refresh_track_row(m_idx);
                        matched += 1;
                    }
                }
            }
            state.set_active_dialog(ActiveDialog::None);
            self.sync_selected_track_inputs(ui);
            self.sync_counts_and_status(
                ui,
                Some(&format!("已从文件名解析 {matched} 首曲目标签（请保存生效）")),
            );
        } else {
            let subset: Vec<AudioTrack> = targets
                .iter()
                .map(|&i| self.working_tracks[i].clone())
                .collect();
            let plan = plan_batch_renames(&subset, template, rename_extra);
            let outcomes = execute_batch_renames(&plan);

            let mut ok_count = 0usize;
            let mut err_count = 0usize;
            for outcome in outcomes {
                let m_idx = targets[outcome.track_index];
                if outcome.error.is_none() {
                    let new_filename = outcome
                        .to
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| self.disk_tracks[m_idx].filename.clone());
                    let new_dir = outcome
                        .to
                        .parent()
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_else(|| self.disk_tracks[m_idx].filedir.clone());

                    self.disk_tracks[m_idx].path = outcome.to.clone();
                    self.disk_tracks[m_idx].filename = new_filename.clone();
                    self.disk_tracks[m_idx].filedir = new_dir.clone();
                    self.working_tracks[m_idx].path = outcome.to;
                    self.working_tracks[m_idx].filename = new_filename;
                    self.working_tracks[m_idx].filedir = new_dir;
                    self.refresh_track_row(m_idx);
                    ok_count += 1;
                } else {
                    err_count += 1;
                }
            }

            state.set_active_dialog(ActiveDialog::None);
            self.sync_selected_track_inputs(ui);
            let note = if err_count > 0 {
                format!("已重命名 {ok_count} 个文件，{err_count} 个失败")
            } else {
                format!("已重命名 {ok_count} 个音频文件")
            };
            self.sync_counts_and_status(ui, Some(&note));
        }
    }

    fn refresh_charset_preview_if_changed(&mut self, ui: &AppWindow, force: bool) {
        let state = ui.global::<AppState>();
        let enc_idx = state.get_charset_encoding_index();
        let m_idx_opt = self.selected_master_index(ui);
        let path_key = m_idx_opt
            .map(|i| self.working_tracks[i].path.to_string_lossy().into_owned())
            .unwrap_or_default();
        let sig = (enc_idx, m_idx_opt, path_key);

        if !force && self.last_charset_preview_sig.as_ref() == Some(&sig) {
            return;
        }
        self.last_charset_preview_sig = Some(sig);

        let Some(m_idx) = m_idx_opt else {
            self.charset_model.set_vec(Vec::new());
            return;
        };

        let label = charset_label_from_index(enc_idx.max(0) as usize);
        let rows = match preview_track_charset(&self.working_tracks[m_idx], label) {
            Ok(list) => list
                .into_iter()
                .map(|r| CharsetPreviewRow {
                    field: r.field_name.into(),
                    original: r.original.into(),
                    converted: r.converted.into(),
                })
                .collect(),
            Err(_) => Vec::new(),
        };
        self.charset_model.set_vec(rows);
    }

    fn apply_charset(&mut self, ui: &AppWindow) {
        let state = ui.global::<AppState>();
        let label = charset_label_from_index(state.get_charset_encoding_index().max(0) as usize);
        let targets = self.batch_target_master_indices(ui);
        let mut changed = 0usize;

        for m_idx in targets {
            if let Ok(patch) = convert_track_charset_patch(&self.working_tracks[m_idx], label) {
                if !patch.is_empty() {
                    merge_tag_patch(&mut self.staged_patches[m_idx], &patch);
                    self.refresh_track_row(m_idx);
                    changed += 1;
                }
            }
        }

        state.set_active_dialog(ActiveDialog::None);
        self.sync_selected_track_inputs(ui);
        self.sync_counts_and_status(
            ui,
            Some(&format!("已按 {label} 修正 {changed} 首曲目编码（请保存生效）")),
        );
    }

    fn apply_batch_lyric_transform(&mut self, ui: &AppWindow, op: &str) {
        let targets = self.batch_target_master_indices(ui);
        if targets.is_empty() {
            return;
        }
        let cfg = read_config_from_ui(ui, self);
        let mut count = 0usize;

        for m_idx in targets {
            let cur_lyrics = self.working_tracks[m_idx].lyrics.clone();
            match op {
                "batch-reformat-timetag" => {
                    if !cur_lyrics.trim().is_empty() {
                        self.staged_patches[m_idx].lyrics =
                            FieldUpdate::Set(reformat_timetags(&cur_lyrics));
                        self.refresh_track_row(m_idx);
                        count += 1;
                    }
                }
                "batch-remove-timetag" => {
                    if !cur_lyrics.trim().is_empty() {
                        self.staged_patches[m_idx].lyrics =
                            FieldUpdate::Set(remove_timetags(&cur_lyrics));
                        self.refresh_track_row(m_idx);
                        count += 1;
                    }
                }
                "batch-delete-head-tag" => {
                    if !cur_lyrics.trim().is_empty() {
                        self.staged_patches[m_idx].lyrics =
                            FieldUpdate::Set(delete_head_tags(&cur_lyrics));
                        self.refresh_track_row(m_idx);
                        count += 1;
                    }
                }
                "batch-delete-blank-lines" => {
                    if !cur_lyrics.trim().is_empty() {
                        self.staged_patches[m_idx].lyrics =
                            FieldUpdate::Set(delete_blank_lines(&cur_lyrics));
                        self.refresh_track_row(m_idx);
                        count += 1;
                    }
                }
                "batch-save-lrc" => {
                    if !cur_lyrics.trim().is_empty()
                        && export_lrc_for_track(&self.working_tracks[m_idx], &cur_lyrics, &cfg)
                            .is_ok()
                    {
                        count += 1;
                    }
                }
                "batch-import-lrc" => {
                    let candidate = resolve_lrc_target_path(&self.working_tracks[m_idx], &cfg);
                    let fallback = self.working_tracks[m_idx].path.with_extension("lrc");
                    let lrc_path = if candidate.is_file() {
                        Some(candidate)
                    } else if fallback.is_file() {
                        Some(fallback)
                    } else {
                        None
                    };
                    if let Some(p) = lrc_path {
                        if let Ok(bytes) = std::fs::read(&p) {
                            let content = decode_lrc_bytes(&bytes, &cfg.lrc_encoding);
                            self.staged_patches[m_idx].lyrics = FieldUpdate::Set(content);
                            self.extra_instrumental[m_idx] = Some(false);
                            self.refresh_track_row(m_idx);
                            count += 1;
                        }
                    }
                }
                _ => {}
            }
        }

        self.sync_selected_track_inputs(ui);
        self.sync_counts_and_status(ui, Some(&format!("批量歌词处理完成 ({count} 首)")));
    }

    fn apply_batch_zhconv(&mut self, ui: &AppWindow, mode: ZhConvMode, filenames_only: bool) {
        let targets = self.batch_target_master_indices(ui);
        if targets.is_empty() {
            return;
        }
        let mut count = 0usize;
        if !filenames_only {
            for m_idx in targets {
                let patch = convert_track_zh_patch(&self.working_tracks[m_idx], mode, true);
                if !patch.is_empty() {
                    merge_tag_patch(&mut self.staged_patches[m_idx], &patch);
                    self.refresh_track_row(m_idx);
                    count += 1;
                }
            }
            self.sync_selected_track_inputs(ui);
            self.sync_counts_and_status(
                ui,
                Some(&format!("已批量转换 {count} 首曲目标签繁简体（请保存生效）")),
            );
        } else {
            for m_idx in targets {
                let old_path = self.disk_tracks[m_idx].path.clone();
                let stem = extract_filename_stem(&self.working_tracks[m_idx].filename);
                let new_stem = sanitize_filename(&convert_text_zh(&stem, mode));
                if new_stem.is_empty() || new_stem == stem {
                    continue;
                }
                let ext = old_path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("");
                let new_filename = if ext.is_empty() {
                    new_stem
                } else {
                    format!("{new_stem}.{ext}")
                };
                let Some(parent) = old_path.parent() else {
                    continue;
                };
                let new_path = parent.join(&new_filename);
                if !new_path.exists() && std::fs::rename(&old_path, &new_path).is_ok() {
                    self.disk_tracks[m_idx].path = new_path.clone();
                    self.disk_tracks[m_idx].filename = new_filename.clone();
                    self.working_tracks[m_idx].path = new_path;
                    self.working_tracks[m_idx].filename = new_filename;
                    self.refresh_track_row(m_idx);
                    count += 1;
                }
            }
            self.sync_selected_track_inputs(ui);
            self.sync_counts_and_status(ui, Some(&format!("已转换 {count} 个文件名繁简体")));
        }
    }

    fn restore_history(&mut self, ui: &AppWindow) {
        let state = ui.global::<AppState>();
        let h_idx = state.get_selected_history_index();
        if h_idx < 0 {
            return;
        }
        let Some(rec) = self.last_history_records.get(h_idx as usize).cloned() else {
            return;
        };

        let target_m = self
            .disk_tracks
            .iter()
            .position(|t| t.path.to_string_lossy() == rec.track_path)
            .or_else(|| self.selected_master_index(ui));

        let Some(m_idx) = target_m else {
            self.sync_counts_and_status(ui, Some("未找到与该历史记录关联的曲目"));
            return;
        };

        let patch = TagPatch {
            title: FieldUpdate::Set(rec.title),
            artist: FieldUpdate::Set(rec.artist),
            album: FieldUpdate::Set(rec.album),
            album_artist: FieldUpdate::Set(rec.album_artist),
            year: FieldUpdate::Set(rec.year),
            track: FieldUpdate::Set(rec.track),
            disc: FieldUpdate::Set(rec.disc),
            genre: FieldUpdate::Set(rec.genre),
            composer: FieldUpdate::Set(rec.composer),
            lyricist: FieldUpdate::Set(rec.lyricist),
            comment: FieldUpdate::Set(rec.comment),
            lyrics: FieldUpdate::Set(rec.lyrics),
            cover: FieldUpdate::Keep,
            replaygain: None,
        };

        merge_tag_patch(&mut self.staged_patches[m_idx], &patch);
        self.refresh_track_row(m_idx);
        state.set_active_dialog(ActiveDialog::None);
        self.sync_selected_track_inputs(ui);
        self.sync_counts_and_status(ui, Some("已还原历史快照到编辑区（请保存生效）"));
    }

    fn remove_items_from_list(&mut self, ui: &AppWindow) {
        let mut targets = self.batch_target_master_indices(ui);
        if targets.is_empty() {
            return;
        }
        targets.sort_unstable();
        targets.dedup();
        let count = targets.len();
        for m_idx in targets.into_iter().rev() {
            self.disk_tracks.remove(m_idx);
            self.working_tracks.remove(m_idx);
            self.staged_patches.remove(m_idx);
            self.staged_cover_previews.remove(m_idx);
            self.extra_modified.remove(m_idx);
            self.extra_instrumental.remove(m_idx);
            self.selected_flags.remove(m_idx);
            self.ui_tracks.remove(m_idx);
        }
        for (i, item) in self.ui_tracks.iter_mut().enumerate() {
            item.id = (i + 1) as i32;
        }
        let state = ui.global::<AppState>();
        let f_idx = state.get_filter_field_index();
        let kw = state.get_filter_keyword().to_string();
        self.rebuild_filtered_tracks(ui, f_idx, &kw);
        self.sync_counts_and_status(ui, Some(&format!("已从列表移除 {count} 首曲目")));
    }

    fn delete_selected_files(&mut self, ui: &AppWindow) {
        let mut targets = self.batch_target_master_indices(ui);
        if targets.is_empty() {
            return;
        }
        targets.sort_unstable();
        targets.dedup();

        let prompt = format!(
            "确定要从磁盘永久删除这 {} 个音频文件吗？此操作不可撤销。",
            targets.len()
        );
        let confirm = rfd::MessageDialog::new()
            .set_level(rfd::MessageLevel::Warning)
            .set_title("确认删除文件")
            .set_description(prompt)
            .set_buttons(rfd::MessageButtons::YesNo)
            .show();

        if confirm != rfd::MessageDialogResult::Yes {
            return;
        }

        let mut deleted_indices = Vec::new();
        let mut fail_count = 0usize;
        for m_idx in targets {
            let path = &self.disk_tracks[m_idx].path;
            match std::fs::remove_file(path) {
                Ok(()) => deleted_indices.push(m_idx),
                Err(err) => {
                    eprintln!("Failed to delete {}: {err}", path.display());
                    fail_count += 1;
                }
            }
        }

        let deleted_count = deleted_indices.len();
        for m_idx in deleted_indices.into_iter().rev() {
            self.disk_tracks.remove(m_idx);
            self.working_tracks.remove(m_idx);
            self.staged_patches.remove(m_idx);
            self.staged_cover_previews.remove(m_idx);
            self.extra_modified.remove(m_idx);
            self.extra_instrumental.remove(m_idx);
            self.selected_flags.remove(m_idx);
            self.ui_tracks.remove(m_idx);
        }
        for (i, item) in self.ui_tracks.iter_mut().enumerate() {
            item.id = (i + 1) as i32;
        }

        let state = ui.global::<AppState>();
        let f_idx = state.get_filter_field_index();
        let kw = state.get_filter_keyword().to_string();
        self.rebuild_filtered_tracks(ui, f_idx, &kw);
        let msg = if fail_count > 0 {
            format!("已删除 {deleted_count} 个文件，{fail_count} 个失败")
        } else {
            format!("已永久删除 {deleted_count} 个文件")
        };
        self.sync_counts_and_status(ui, Some(&msg));
    }
}

fn wire_callbacks(ui: &AppWindow, ctrl: Rc<RefCell<BackendController>>) {
    let logic = ui.global::<AppLogic>();

    {
        let ui_w = ui.as_weak();
        let ctrl = ctrl.clone();
        logic.on_open_dialog(move |dialog| {
            let Some(ui) = ui_w.upgrade() else { return };
            let mut c = ctrl.borrow_mut();
            let state = ui.global::<AppState>();
            let prev_dialog = state.get_active_dialog();

            match dialog {
                ActiveDialog::ComboSearch | ActiveDialog::LyricSearch | ActiveDialog::CoverSearch => {
                    let cfg = read_config_from_ui(&ui, &c);
                    if let Some(m_idx) = c
                        .selected_master_index(&ui)
                        .or_else(|| c.checked_master_indices().into_iter().next())
                    {
                        let track = &c.working_tracks[m_idx];
                        let title = if cfg.search_cond_title_mode == 1 || track.title.trim().is_empty()
                        {
                            extract_filename_stem(&track.filename)
                        } else {
                            track.title.clone()
                        };
                        state.set_combo_query_title(title.into());
                        state.set_combo_query_artist(if cfg.search_use_artist {
                            track.artist.clone().into()
                        } else {
                            "".into()
                        });
                        state.set_combo_query_album(if cfg.search_use_album {
                            track.album.clone().into()
                        } else {
                            "".into()
                        });
                        state.set_combo_query_id(track.netease_id.clone().into());
                    } else {
                        state.set_combo_query_title("".into());
                        state.set_combo_query_artist("".into());
                        state.set_combo_query_album("".into());
                        state.set_combo_query_id("".into());
                    }

                    state.set_active_dialog(dialog);
                    match dialog {
                        ActiveDialog::ComboSearch => c.spawn_combo_or_lyric_search(&ui, false),
                        ActiveDialog::LyricSearch => c.spawn_combo_or_lyric_search(&ui, true),
                        ActiveDialog::CoverSearch => c.spawn_cover_search(&ui),
                        _ => {}
                    }
                    return;
                }
                ActiveDialog::LyricEditor => {
                    if prev_dialog == ActiveDialog::None {
                        if let Some(m_idx) = c.selected_master_index(&ui) {
                            state.set_lyric_editor_text(
                                c.working_tracks[m_idx].lyrics.clone().into(),
                            );
                        }
                    }
                }
                ActiveDialog::AdjustTimetag => {
                    c.adjust_from_editor = prev_dialog == ActiveDialog::LyricEditor;
                    if !c.adjust_from_editor {
                        if let Some(m_idx) = c.selected_master_index(&ui) {
                            state.set_lyric_editor_text(
                                c.working_tracks[m_idx].lyrics.clone().into(),
                            );
                        }
                    }
                }
                ActiveDialog::FindReplace => {
                    c.find_replace_from_editor = prev_dialog == ActiveDialog::LyricEditor;
                }
                ActiveDialog::FilenameRel => {
                    c.refresh_filename_previews_if_changed(&ui, true);
                }
                ActiveDialog::CharsetFix => {
                    c.refresh_charset_preview_if_changed(&ui, true);
                }
                ActiveDialog::TagsHistory => {
                    c.reload_history_from_db();
                }
                ActiveDialog::Options => {
                    c.refresh_sources_model(&ui);
                }
                _ => {}
            }

            state.set_active_dialog(dialog);
        });
    }

    {
        let ui_w = ui.as_weak();
        let ctrl = ctrl.clone();
        logic.on_close_dialog(move || {
            let Some(ui) = ui_w.upgrade() else { return };
            let mut c = ctrl.borrow_mut();
            let state = ui.global::<AppState>();
            let cur = state.get_active_dialog();

            if cur == ActiveDialog::Progress {
                if let Some(ref flag) = c.active_rg_cancel {
                    flag.store(true, Ordering::SeqCst);
                }
            } else if cur == ActiveDialog::AutoMatch {
                if let Some(ref flag) = c.active_am_cancel {
                    flag.store(true, Ordering::SeqCst);
                }
            } else if matches!(
                cur,
                ActiveDialog::ComboSearch | ActiveDialog::LyricSearch | ActiveDialog::CoverSearch
            ) {
                c.search_gen = c.search_gen.wrapping_add(1);
                state.set_is_searching(false);
            }

            if cur == ActiveDialog::FindReplace && c.find_replace_from_editor {
                c.find_replace_from_editor = false;
                state.set_active_dialog(ActiveDialog::LyricEditor);
                return;
            }
            if cur == ActiveDialog::AdjustTimetag && c.adjust_from_editor {
                c.adjust_from_editor = false;
                state.set_active_dialog(ActiveDialog::LyricEditor);
                return;
            }

            state.set_active_dialog(ActiveDialog::None);
            ui.invoke_focus_main();
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
                let m_idx = c.filtered_indices[idx as usize];
                let fname = c.working_tracks[m_idx].filename.clone();
                c.sync_counts_and_status(&ui, Some(&fname));
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
                    c.selected_flags[m_idx] = !c.selected_flags[m_idx];
                    c.refresh_track_row(m_idx);
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
            for m_idx in indices {
                c.selected_flags[m_idx] = true;
                c.ui_tracks[m_idx].selected = true;
            }
            c.sync_visible_tracks();
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
            for m_idx in indices {
                c.selected_flags[m_idx] = false;
                c.ui_tracks[m_idx].selected = false;
            }
            c.sync_visible_tracks();
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
            for m_idx in indices {
                let next = !c.selected_flags[m_idx];
                c.selected_flags[m_idx] = next;
                c.ui_tracks[m_idx].selected = next;
            }
            c.sync_visible_tracks();
            c.sync_selected_track_inputs(&ui);
            c.sync_counts_and_status(&ui, Some("已反选"));
        });
    }

    {
        let ui_w = ui.as_weak();
        let ctrl = ctrl.clone();
        logic.on_save_selected(move || {
            let Some(ui) = ui_w.upgrade() else { return };
            ctrl.borrow_mut().save_selected(&ui);
        });
    }

    {
        let ui_w = ui.as_weak();
        let ctrl = ctrl.clone();
        logic.on_revert_selected(move || {
            let Some(ui) = ui_w.upgrade() else { return };
            ctrl.borrow_mut().revert_selected(&ui);
        });
    }

    {
        let ui_w = ui.as_weak();
        let ctrl = ctrl.clone();
        logic.on_set_theme(move |pref| {
            let Some(ui) = ui_w.upgrade() else { return };
            let variant = theme_pref_from_i32(pref);
            let label = match pref {
                0 => "跟随系统主题",
                1 => "浅色主题",
                _ => "深色主题",
            };
            ui.global::<Theme>().set_preference(variant);
            let mut c = ctrl.borrow_mut();
            c.config.theme_preference = pref;
            let _ = c.config.save_default();
            c.sync_counts_and_status(&ui, Some(label));
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
            ctrl.borrow_mut()
                .edit_selected_field(&ui, field.as_str(), value);
        });
    }

    {
        let ui_w = ui.as_weak();
        let ctrl = ctrl.clone();
        logic.on_set_field_mode(move |field, mode| {
            let Some(ui) = ui_w.upgrade() else { return };
            ctrl.borrow_mut().set_field_mode(&ui, field.as_str(), mode);
        });
    }

    {
        let ui_w = ui.as_weak();
        let ctrl = ctrl.clone();
        logic.on_apply_combo_candidate(move |cand_idx, save_immediately| {
            let Some(ui) = ui_w.upgrade() else { return };
            ctrl.borrow_mut()
                .spawn_apply_combo_candidate(&ui, cand_idx, save_immediately);
        });
    }

    {
        let ui_w = ui.as_weak();
        let ctrl = ctrl.clone();
        logic.on_apply_lyric_candidate(move |cand_idx| {
            let Some(ui) = ui_w.upgrade() else { return };
            ctrl.borrow_mut()
                .spawn_apply_lyric_candidate(&ui, cand_idx);
        });
    }

    {
        let ui_w = ui.as_weak();
        let ctrl = ctrl.clone();
        logic.on_apply_cover_candidate(move |cov_idx| {
            let Some(ui) = ui_w.upgrade() else { return };
            ctrl.borrow_mut().spawn_apply_cover_candidate(&ui, cov_idx);
        });
    }

    {
        let ui_w = ui.as_weak();
        let ctrl = ctrl.clone();
        logic.on_lyric_action(move |action| {
            let Some(ui) = ui_w.upgrade() else { return };
            ctrl.borrow_mut().handle_lyric_action(&ui, action.as_str());
        });
    }

    {
        let ui_w = ui.as_weak();
        let ctrl = ctrl.clone();
        logic.on_move_column(move |idx, delta| {
            let Some(ui) = ui_w.upgrade() else { return };
            let mut c = ctrl.borrow_mut();
            let count = c.columns_model.row_count() as i32;
            let target = idx + delta;
            if idx >= 0 && target >= 0 && idx < count && target < count {
                let a = c.columns_model.row_data(idx as usize).unwrap();
                let b = c.columns_model.row_data(target as usize).unwrap();
                c.columns_model.set_row_data(idx as usize, b);
                c.columns_model.set_row_data(target as usize, a);
                ui.global::<AppState>().set_selected_column_index(target);
                c.config = read_config_from_ui(&ui, &c);
                let _ = c.config.save_default();
            }
        });
    }

    {
        let ui_w = ui.as_weak();
        let ctrl = ctrl.clone();
        logic.on_reset_columns(move || {
            let Some(ui) = ui_w.upgrade() else { return };
            let mut c = ctrl.borrow_mut();
            c.columns_model.set_vec(default_columns());
            ui.global::<AppState>().set_selected_column_index(0);
            c.config = read_config_from_ui(&ui, &c);
            let _ = c.config.save_default();
            c.sync_counts_and_status(&ui, Some("显示列已重置为默认"));
        });
    }

    {
        let ui_w = ui.as_weak();
        let ctrl = ctrl.clone();
        logic.on_toggle_column(move |idx| {
            let Some(ui) = ui_w.upgrade() else { return };
            let mut c = ctrl.borrow_mut();
            if idx >= 0 && (idx as usize) < c.columns_model.row_count() {
                if let Some(mut col) = c.columns_model.row_data(idx as usize) {
                    col.visible = !col.visible;
                    c.columns_model.set_row_data(idx as usize, col);
                    ui.global::<AppState>().set_selected_column_index(idx);
                    c.config = read_config_from_ui(&ui, &c);
                    let _ = c.config.save_default();
                }
            }
        });
    }

    {
        let ui_w = ui.as_weak();
        let ctrl = ctrl.clone();
        logic.on_toggle_source(move |idx| {
            let Some(ui) = ui_w.upgrade() else { return };
            let mut c = ctrl.borrow_mut();
            if idx >= 0 {
                if let Some(item) = c.sources_model.row_data(idx as usize) {
                    let next = !item.enabled;
                    c.source_registry.set_enabled(item.id.as_str(), next);
                    c.refresh_sources_model(&ui);
                    let label = if next { "启用" } else { "停用" };
                    c.sync_counts_and_status(
                        &ui,
                        Some(&format!("已{label}标签源: {}", item.name)),
                    );
                }
            }
        });
    }

    {
        let ui_w = ui.as_weak();
        let ctrl = ctrl.clone();
        logic.on_move_source(move |idx, delta| {
            let Some(ui) = ui_w.upgrade() else { return };
            let mut c = ctrl.borrow_mut();
            let mut ids: Vec<String> = c
                .source_registry
                .list()
                .into_iter()
                .map(|s| s.id)
                .collect();
            let target = idx + delta;
            if idx >= 0 && target >= 0 && (idx as usize) < ids.len() && (target as usize) < ids.len() {
                ids.swap(idx as usize, target as usize);
                c.source_registry.reorder(&ids);
                ui.global::<AppState>().set_selected_source_index(target);
                c.refresh_sources_model(&ui);
                c.sync_counts_and_status(&ui, Some("已调整标签源匹配优先级"));
            }
        });
    }

    {
        let ui_w = ui.as_weak();
        let ctrl = ctrl.clone();
        logic.on_reload_plugins(move || {
            let Some(ui) = ui_w.upgrade() else { return };
            let mut c = ctrl.borrow_mut();
            let prev_cfg = read_config_from_ui(&ui, &c);
            let plugin_dirs = plugin_search_dirs();
            let mut reg = build_default_source_registry(&plugin_dirs, &prev_cfg.artist_sep);
            sync_registry_with_config(&mut reg, &prev_cfg);
            let total = reg.list().len();
            let js_count = reg.list().iter().filter(|s| !s.builtin).count();
            c.source_registry = reg;
            c.refresh_sources_model(&ui);
            c.sync_counts_and_status(
                &ui,
                Some(&format!("已重新扫描插件：共 {total} 个标签源（含 {js_count} 个 QuickJS 插件）")),
            );
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
                "change-dir" => {
                    let mut dlg = rfd::FileDialog::new().set_title("选择工作目录");
                    if !c.config.working_dir.is_empty() {
                        let p = Path::new(&c.config.working_dir);
                        if p.is_dir() {
                            dlg = dlg.set_directory(p);
                        }
                    }
                    if let Some(folder) = dlg.pick_folder() {
                        let dir_str = folder.to_string_lossy().into_owned();
                        c.explicit_files.clear();
                        c.config.working_dir = dir_str.clone();
                        c.config.dirs = vec![DirConfigEntry {
                            path: dir_str,
                            include_subdirs: true,
                        }];
                        c.sync_dirs_model(&ui, None);
                        let _ = c.config.save_default();
                        c.spawn_directory_scan(&ui);
                    }
                }
                "add-dir" | "add-folder" => {
                    let mut dlg = rfd::FileDialog::new().set_title("添加工作目录");
                    if !c.config.working_dir.is_empty() {
                        let p = Path::new(&c.config.working_dir);
                        if p.is_dir() {
                            dlg = dlg.set_directory(p);
                        }
                    }
                    if let Some(folder) = dlg.pick_folder() {
                        let dir_str = folder.to_string_lossy().into_owned();
                        if !c.config.dirs.iter().any(|d| d.path == dir_str) {
                            c.config.dirs.push(DirConfigEntry {
                                path: dir_str.clone(),
                                include_subdirs: true,
                            });
                            if c.config.working_dir.is_empty() {
                                c.config.working_dir = dir_str;
                            }
                            c.sync_dirs_model(&ui, None);
                            let _ = c.config.save_default();
                            c.spawn_directory_scan(&ui);
                        } else {
                            c.sync_counts_and_status(&ui, Some("该目录已在工作目录列表中"));
                        }
                    }
                }
                "remove-dir" => {
                    let d_idx = state.get_selected_dir_index();
                    if d_idx >= 0 && (d_idx as usize) < c.config.dirs.len() {
                        c.config.dirs.remove(d_idx as usize);
                        c.config.working_dir = c
                            .config
                            .dirs
                            .first()
                            .map(|d| d.path.clone())
                            .unwrap_or_default();
                        state.set_selected_dir_index(0);
                        c.sync_dirs_model(&ui, None);
                        let _ = c.config.save_default();
                        c.spawn_directory_scan(&ui);
                    }
                }
                "toggle-dir-subdirs" => {
                    let d_idx = state.get_selected_dir_index();
                    if d_idx >= 0 && (d_idx as usize) < c.config.dirs.len() {
                        c.config.dirs[d_idx as usize].include_subdirs =
                            !c.config.dirs[d_idx as usize].include_subdirs;
                        c.sync_dirs_model(&ui, None);
                        let _ = c.config.save_default();
                        c.spawn_directory_scan(&ui);
                    }
                }
                "refresh" => {
                    c.spawn_directory_scan(&ui);
                }
                "read-tags" => {
                    c.reload_selected_from_disk(&ui);
                }
                "clear-tags" => {
                    c.clear_tags_selected(&ui);
                }
                "delete-cover" => {
                    c.set_field_mode(&ui, "cover", 2);
                }
                "extract-cover" => {
                    c.extract_cover(&ui);
                }
                "choose-local-cover" => {
                    c.choose_local_cover(&ui);
                }
                "apply-cover-resolution" => {
                    c.apply_cover_resolution(&ui);
                }
                "search-combo" => {
                    c.spawn_combo_or_lyric_search(&ui, false);
                }
                "search-lyric" => {
                    c.spawn_combo_or_lyric_search(&ui, true);
                }
                "search-cover" => {
                    c.spawn_cover_search(&ui);
                }
                "auto-match" => {
                    c.spawn_auto_match(&ui);
                }
                "replaygain" => {
                    c.spawn_replaygain_scan(&ui, None);
                }
                "replaygain-track" => {
                    c.spawn_replaygain_scan(&ui, Some(RgScanMode::TrackOnly));
                }
                "replaygain-album" => {
                    c.spawn_replaygain_scan(&ui, Some(RgScanMode::AlbumAndTrack));
                }
                "cancel-progress" => {
                    if let Some(ref flag) = c.active_rg_cancel {
                        flag.store(true, Ordering::SeqCst);
                    }
                    if let Some(ref flag) = c.active_am_cancel {
                        flag.store(true, Ordering::SeqCst);
                    }
                }
                "find-prev" | "find-next" | "replace-one" | "find-replace" => {
                    c.handle_find_replace(&ui, action.as_str());
                }
                "apply-filename" => {
                    c.apply_filename(&ui);
                }
                "apply-charset" => {
                    c.apply_charset(&ui);
                }
                "batch-reformat-timetag"
                | "batch-remove-timetag"
                | "batch-delete-head-tag"
                | "batch-delete-blank-lines"
                | "batch-save-lrc"
                | "batch-import-lrc" => {
                    c.apply_batch_lyric_transform(&ui, action.as_str());
                }
                "tags-cht-to-chs" => {
                    c.apply_batch_zhconv(&ui, ZhConvMode::ToSimplified, false);
                }
                "tags-chs-to-cht" => {
                    c.apply_batch_zhconv(&ui, ZhConvMode::ToTraditional, false);
                }
                "filename-cht-to-chs" => {
                    c.apply_batch_zhconv(&ui, ZhConvMode::ToSimplified, true);
                }
                "filename-chs-to-cht" => {
                    c.apply_batch_zhconv(&ui, ZhConvMode::ToTraditional, true);
                }
                "restore-history" => {
                    c.restore_history(&ui);
                }
                "clear-history" => {
                    let _ = c.history_store.clear();
                    c.last_history_records.clear();
                    c.history_model.set_vec(Vec::new());
                    c.sync_counts_and_status(&ui, Some("已清除所有标签历史记录"));
                }
                "apply-options" => {
                    let old_exts = c.config.file_exts.clone();
                    let old_ignore_video = c.config.ignore_video;
                    let old_duration = c.config.duration_filter;
                    let old_sep = c.config.artist_sep.clone();

                    let new_cfg = read_config_from_ui(&ui, &c);
                    c.config = new_cfg;

                    if c.config.artist_sep != old_sep {
                        let plugin_dirs = plugin_search_dirs();
                        let mut reg =
                            build_default_source_registry(&plugin_dirs, &c.config.artist_sep);
                        sync_registry_with_config(&mut reg, &c.config);
                        c.source_registry = reg;
                        c.refresh_sources_model(&ui);
                    }

                    let save_res = c.config.save_default();
                    state.set_active_dialog(ActiveDialog::None);
                    ui.invoke_focus_main();

                    let needs_rescan = c.config.file_exts != old_exts
                        || c.config.ignore_video != old_ignore_video
                        || c.config.duration_filter != old_duration;
                    if needs_rescan && (!c.config.dirs.is_empty() || !c.explicit_files.is_empty()) {
                        c.spawn_directory_scan(&ui);
                    }

                    match save_res {
                        Ok(p) => c.sync_counts_and_status(
                            &ui,
                            Some(&format!("选项设置已保存 ({})", p.display())),
                        ),
                        Err(err) => c.sync_counts_and_status(
                            &ui,
                            Some(&format!("选项已在本次生效（保存配置文件失败: {err:#}）")),
                        ),
                    }
                }
                "reset-options" => {
                    let mut defaults = AppConfig::default();
                    defaults.working_dir = c.config.working_dir.clone();
                    defaults.dirs = c.config.dirs.clone();
                    defaults.columns = c.config.columns.clone();
                    c.config = defaults;

                    let plugin_dirs = plugin_search_dirs();
                    let mut reg =
                        build_default_source_registry(&plugin_dirs, &c.config.artist_sep);
                    sync_registry_with_config(&mut reg, &c.config);
                    c.source_registry = reg;

                    apply_config_to_ui(&ui, &c.config);
                    c.refresh_sources_model(&ui);
                    c.sync_counts_and_status(&ui, Some("选项已还原为默认值"));
                }
                "open-file-dir" => {
                    if let Some(m_idx) = c.selected_master_index(&ui) {
                        let dir = &c.working_tracks[m_idx].filedir;
                        if let Err(err) = open_directory_in_file_manager(Path::new(dir)) {
                            c.sync_counts_and_status(
                                &ui,
                                Some(&format!("打开目录失败: {err:#}")),
                            );
                        } else {
                            c.sync_counts_and_status(&ui, Some(&format!("已打开目录: {dir}")));
                        }
                    }
                }
                "remove-items" => {
                    c.remove_items_from_list(&ui);
                }
                "delete-files" => {
                    c.delete_selected_files(&ui);
                }
                _ => {
                    c.sync_counts_and_status(&ui, Some(&format!("未知操作: {action}")));
                }
            }
        });
    }
}

pub fn collect_audio_files(
    dirs: &[DirConfigEntry],
    explicit_files: &[PathBuf],
    format_registry: &FormatRegistry,
    allowed_exts: &[String],
    ignore_video: bool,
) -> (Vec<PathBuf>, Vec<usize>) {
    let mut seen = HashSet::new();
    let mut collected = Vec::new();
    let mut per_dir_counts = Vec::with_capacity(dirs.len());

    let is_allowed = |path: &Path| -> bool {
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            return false;
        };
        let norm = ext.to_ascii_lowercase();
        if ignore_video && norm == "mp4" {
            return false;
        }
        if format_registry.by_extension(&norm).is_none() {
            return false;
        }
        if !allowed_exts.is_empty() && !allowed_exts.iter().any(|e| e == &norm) {
            return false;
        }
        true
    };

    for dir_entry in dirs {
        let root = Path::new(&dir_entry.path);
        if !root.is_dir() {
            per_dir_counts.push(0);
            continue;
        }
        let max_depth = if dir_entry.include_subdirs {
            usize::MAX
        } else {
            1
        };
        let mut dir_count = 0usize;
        for entry in WalkDir::new(root)
            .max_depth(max_depth)
            .follow_links(false)
            .into_iter()
            .filter_map(Result::ok)
        {
            let path = entry.path();
            if path.is_file() && is_allowed(path) {
                dir_count += 1;
                let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
                if seen.insert(key) {
                    collected.push(path.to_path_buf());
                }
            }
        }
        per_dir_counts.push(dir_count);
    }

    for file_path in explicit_files {
        if file_path.is_file() && is_allowed(file_path) {
            let key = std::fs::canonicalize(file_path).unwrap_or_else(|_| file_path.clone());
            if seen.insert(key) {
                collected.push(file_path.clone());
            }
        }
    }

    (collected, per_dir_counts)
}

fn cover_to_slint_image(cover: Option<&CoverData>) -> slint::Image {
    let Some(c) = cover else {
        return slint::Image::default();
    };
    if c.thumb_width == 0 || c.thumb_height == 0 {
        return slint::Image::default();
    }
    let expected = (c.thumb_width as usize)
        .saturating_mul(c.thumb_height as usize)
        .saturating_mul(4);
    if c.rgba_thumb.len() != expected {
        return slint::Image::default();
    }
    let buf = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
        &c.rgba_thumb,
        c.thumb_width,
        c.thumb_height,
    );
    slint::Image::from_rgba8(buf)
}

fn decoded_to_slint_image(decoded: Option<&DecodedRgba>) -> slint::Image {
    let Some(d) = decoded else {
        return slint::Image::default();
    };
    if d.width == 0 || d.height == 0 {
        return slint::Image::default();
    }
    let expected = (d.width as usize)
        .saturating_mul(d.height as usize)
        .saturating_mul(4);
    if d.rgba.len() != expected {
        return slint::Image::default();
    }
    let buf = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&d.rgba, d.width, d.height);
    slint::Image::from_rgba8(buf)
}

fn decode_rgba_thumb(bytes: &[u8]) -> Option<DecodedRgba> {
    let img = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()?;
    let (w, h) = img.dimensions();
    let thumb: DynamicImage = if w > THUMB_MAX_EDGE || h > THUMB_MAX_EDGE {
        img.thumbnail(THUMB_MAX_EDGE, THUMB_MAX_EDGE)
    } else {
        img
    };
    let rgba = thumb.into_rgba8();
    let width = rgba.width();
    let height = rgba.height();
    Some(DecodedRgba {
        rgba: rgba.into_raw(),
        width,
        height,
    })
}

pub fn build_cover_data_from_bytes(bytes: Vec<u8>, mime_hint: &str) -> Result<CoverData> {
    if bytes.is_empty() {
        bail!("cover image bytes are empty");
    }
    let guessed = image::guess_format(&bytes).ok();
    let mime = if !mime_hint.trim().is_empty() {
        mime_hint.to_string()
    } else {
        match guessed {
            Some(ImageFormat::Png) => "image/png".to_string(),
            Some(ImageFormat::WebP) => "image/webp".to_string(),
            Some(ImageFormat::Gif) => "image/gif".to_string(),
            Some(ImageFormat::Bmp) => "image/bmp".to_string(),
            _ => "image/jpeg".to_string(),
        }
    };

    let img = ImageReader::new(Cursor::new(&bytes))
        .with_guessed_format()
        .context("failed to detect cover image format")?
        .decode()
        .context("failed to decode cover image")?;
    let (width, height) = img.dimensions();
    let thumb = if width > THUMB_MAX_EDGE || height > THUMB_MAX_EDGE {
        img.thumbnail(THUMB_MAX_EDGE, THUMB_MAX_EDGE)
    } else {
        img
    };
    let rgba = thumb.into_rgba8();
    let thumb_width = rgba.width();
    let thumb_height = rgba.height();
    let size_bytes = bytes.len();

    Ok(CoverData {
        bytes,
        mime,
        width,
        height,
        rgba_thumb: rgba.into_raw(),
        thumb_width,
        thumb_height,
        size_bytes,
    })
}

fn download_candidate_thumb(cand: &CandidateTrack, src_opt: Option<&dyn TagSource>) -> Option<DecodedRgba> {
    if let Some(ref bytes) = cand.cover_bytes {
        return decode_rgba_thumb(bytes);
    }
    let url = cand.cover_url.as_deref()?.trim();
    if url.is_empty() {
        return None;
    }
    let thumb_url = if url.contains("music.126.net") && !url.contains('?') {
        format!("{url}?param=130y130")
    } else {
        url.to_string()
    };
    let src = src_opt?;
    let bytes = src
        .download_bytes(&thumb_url)
        .or_else(|_| src.download_bytes(url))
        .ok()?;
    decode_rgba_thumb(&bytes)
}

fn audio_track_to_track_item(
    id: i32,
    track: &AudioTrack,
    modified: bool,
    selected: bool,
) -> TrackItem {
    TrackItem {
        id,
        filename: track.filename.clone().into(),
        filedir: track.filedir.clone().into(),
        tag_types: track.tag_types.clone().into(),
        title: track.title.clone().into(),
        artist: track.artist.clone().into(),
        album: track.album.clone().into(),
        album_artist: track.album_artist.clone().into(),
        year: track.year.clone().into(),
        track_str: track.track.clone().into(),
        disc_str: track.disc.clone().into(),
        genre: track.genre.clone().into(),
        composer: track.composer.clone().into(),
        lyricist: track.lyricist.clone().into(),
        comment: track.comment.clone().into(),
        has_163_key: track.has_163_key,
        netease_id: track.netease_id.clone().into(),
        has_picture: track.cover.is_some(),
        cover_info: track
            .cover
            .as_ref()
            .map(CoverData::format_info)
            .unwrap_or_default()
            .into(),
        cover: cover_to_slint_image(track.cover.as_ref()),
        has_lyrics: !track.lyrics.trim().is_empty(),
        lyrics_preview: first_lyric_line(&track.lyrics).into(),
        lyrics: track.lyrics.clone().into(),
        is_instrumental: track.is_instrumental,
        rg_track_gain: track.replaygain.track_gain.clone().unwrap_or_default().into(),
        rg_track_peak: track.replaygain.track_peak.clone().unwrap_or_default().into(),
        rg_album_gain: track.replaygain.album_gain.clone().unwrap_or_default().into(),
        rg_album_peak: track.replaygain.album_peak.clone().unwrap_or_default().into(),
        bit_per_sample: if track.bit_per_sample > 0 {
            format!("{} bit", track.bit_per_sample).into()
        } else {
            "".into()
        },
        channels: if track.channels > 0 {
            track.channels.to_string().into()
        } else {
            "".into()
        },
        sample_rate: if track.sample_rate > 0 {
            format!("{} Hz", track.sample_rate).into()
        } else {
            "".into()
        },
        bitrate: if track.bitrate > 0 {
            format!("{} kbps", track.bitrate).into()
        } else {
            "".into()
        },
        duration: track.duration_formatted().into(),
        update_time: track.update_time.clone().into(),
        modified,
        selected,
    }
}

pub fn merge_tag_patch(dst: &mut TagPatch, src: &TagPatch) {
    fn merge_field<T: Clone>(dst_f: &mut FieldUpdate<T>, src_f: &FieldUpdate<T>) {
        if !src_f.is_keep() {
            *dst_f = src_f.clone();
        }
    }

    merge_field(&mut dst.title, &src.title);
    merge_field(&mut dst.artist, &src.artist);
    merge_field(&mut dst.album, &src.album);
    merge_field(&mut dst.album_artist, &src.album_artist);
    merge_field(&mut dst.year, &src.year);
    merge_field(&mut dst.track, &src.track);
    merge_field(&mut dst.disc, &src.disc);
    merge_field(&mut dst.genre, &src.genre);
    merge_field(&mut dst.composer, &src.composer);
    merge_field(&mut dst.lyricist, &src.lyricist);
    merge_field(&mut dst.comment, &src.comment);
    merge_field(&mut dst.cover, &src.cover);
    merge_field(&mut dst.lyrics, &src.lyrics);
    if src.replaygain.is_some() {
        dst.replaygain = src.replaygain.clone();
    }
}

pub fn apply_patch_to_track(
    disk: &AudioTrack,
    patch: &TagPatch,
    cover_preview: Option<&CoverData>,
) -> AudioTrack {
    fn apply_str(orig: &str, upd: &FieldUpdate<String>) -> String {
        match upd {
            FieldUpdate::Keep => orig.to_string(),
            FieldUpdate::Set(v) => v.clone(),
            FieldUpdate::Clear => String::new(),
        }
    }

    let mut out = disk.clone();
    out.title = apply_str(&disk.title, &patch.title);
    out.artist = apply_str(&disk.artist, &patch.artist);
    out.album = apply_str(&disk.album, &patch.album);
    out.album_artist = apply_str(&disk.album_artist, &patch.album_artist);
    out.year = apply_str(&disk.year, &patch.year);
    out.track = apply_str(&disk.track, &patch.track);
    out.disc = apply_str(&disk.disc, &patch.disc);
    out.genre = apply_str(&disk.genre, &patch.genre);
    out.composer = apply_str(&disk.composer, &patch.composer);
    out.lyricist = apply_str(&disk.lyricist, &patch.lyricist);
    out.comment = apply_str(&disk.comment, &patch.comment);
    out.lyrics = apply_str(&disk.lyrics, &patch.lyrics);

    match &patch.comment {
        FieldUpdate::Keep => {}
        FieldUpdate::Clear => {
            out.has_163_key = false;
            out.netease_id.clear();
        }
        FieldUpdate::Set(c) => {
            out.has_163_key = c.contains(KEY_163_PREFIX);
            out.netease_id = extract_163_song_id(c).unwrap_or_default();
        }
    }

    match &patch.cover {
        FieldUpdate::Keep => {}
        FieldUpdate::Clear => {
            out.cover = None;
        }
        FieldUpdate::Set(_) => {
            if let Some(cd) = cover_preview {
                out.cover = Some(cd.clone());
            }
        }
    }

    if !patch.lyrics.is_keep() {
        out.is_instrumental = matches!(
            process_netease_lyrics(
                Some(&out.lyrics),
                None,
                false,
                &out.title,
                &LyricProcessOptions::default()
            ),
            ProcessedLyrics::InstrumentalSkipped
        );
    }

    if let Some(ref rg) = patch.replaygain {
        out.replaygain = rg.clone();
    }

    out
}

fn patch_field_mode(patch: &TagPatch, field: &str) -> i32 {
    let upd = match field {
        "title" => &patch.title,
        "artist" => &patch.artist,
        "album" => &patch.album,
        "album-artist" => &patch.album_artist,
        "year" => &patch.year,
        "track-str" => &patch.track,
        "disc-str" => &patch.disc,
        "genre" => &patch.genre,
        "composer" => &patch.composer,
        "lyricist" => &patch.lyricist,
        "comment" => &patch.comment,
        _ => return 1,
    };
    match upd {
        FieldUpdate::Set(_) => 0,
        FieldUpdate::Keep => 1,
        FieldUpdate::Clear => 2,
    }
}

fn set_patch_text_field(patch: &mut TagPatch, field: &str, value: FieldUpdate<String>) {
    match field {
        "title" => patch.title = value,
        "artist" => patch.artist = value,
        "album" => patch.album = value,
        "album-artist" => patch.album_artist = value,
        "year" => patch.year = value,
        "track-str" => patch.track = value,
        "disc-str" => patch.disc = value,
        "genre" => patch.genre = value,
        "composer" => patch.composer = value,
        "lyricist" => patch.lyricist = value,
        "comment" => patch.comment = value,
        _ => {}
    }
}

fn track_text_field(track: &AudioTrack, field: &str) -> String {
    match field {
        "title" => track.title.clone(),
        "artist" => track.artist.clone(),
        "album" => track.album.clone(),
        "album-artist" => track.album_artist.clone(),
        "year" => track.year.clone(),
        "track-str" => track.track.clone(),
        "disc-str" => track.disc.clone(),
        "genre" => track.genre.clone(),
        "composer" => track.composer.clone(),
        "lyricist" => track.lyricist.clone(),
        "comment" => track.comment.clone(),
        _ => String::new(),
    }
}

fn ui_track_text_field(track: &TrackItem, field: &str) -> SharedString {
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

fn merge_candidate_detail(base: &mut CandidateTrack, detail: CandidateTrack) {
    if !detail.title.is_empty() {
        base.title = detail.title;
    }
    if !detail.artist.is_empty() {
        base.artist = detail.artist;
    }
    if !detail.album.is_empty() {
        base.album = detail.album;
    }
    if !detail.album_artist.is_empty() {
        base.album_artist = detail.album_artist;
    }
    if !detail.year.is_empty() {
        base.year = detail.year;
    }
    if !detail.track.is_empty() {
        base.track = detail.track;
    }
    if !detail.disc.is_empty() {
        base.disc = detail.disc;
    }
    if !detail.genre.is_empty() {
        base.genre = detail.genre;
    }
    if !detail.composer.is_empty() {
        base.composer = detail.composer;
    }
    if !detail.lyricist.is_empty() {
        base.lyricist = detail.lyricist;
    }
    if !detail.comment.is_empty() {
        base.comment = detail.comment;
    }
    if detail.cover_url.is_some() {
        base.cover_url = detail.cover_url;
    }
    if detail.cover_bytes.is_some() {
        base.cover_bytes = detail.cover_bytes;
    }
    if detail.netease_meta.is_some() {
        base.netease_meta = detail.netease_meta;
    }
    if detail.raw_lrc.is_some() {
        base.raw_lrc = detail.raw_lrc;
    }
    if detail.raw_tlyric.is_some() {
        base.raw_tlyric = detail.raw_tlyric;
    }
    base.nolyric = base.nolyric || detail.nolyric;
    base.is_instrumental = base.is_instrumental || detail.is_instrumental;
    base.has_translation = base.has_translation || detail.has_translation;
}

fn normalize_optional_disc(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some((num_part, total_part)) = trimmed.split_once('/') {
        let num_str = num_part.trim();
        let total_str = total_part.trim();
        if let (Ok(num), Ok(total)) = (num_str.parse::<u32>(), total_str.parse::<u32>()) {
            if num == 0 || (num <= 1 && total <= 1) {
                return None;
            }
            return Some(trimmed.to_string());
        }
        return Some(trimmed.to_string());
    }
    if trimmed.bytes().all(|b| b.is_ascii_digit()) {
        if let Ok(n) = trimmed.parse::<u32>() {
            return (n >= 2).then(|| n.to_string());
        }
    }
    Some(trimmed.to_string())
}

fn normalize_optional_positive_num(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some((num_part, _)) = trimmed.split_once('/') {
        let num_str = num_part.trim();
        if num_str.is_empty() || num_str.bytes().all(|b| b == b'0') {
            return None;
        }
        return Some(trimmed.to_string());
    }
    if trimmed.bytes().all(|b| b == b'0') {
        return None;
    }
    Some(trimmed.to_string())
}

fn normalize_optional_album_artist(album_artist: &str, artist: &str) -> Option<String> {
    let trimmed = album_artist.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case(artist.trim()) {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn build_combo_patch(
    cand: &CandidateTrack,
    flags: &ComboOverwriteFlags,
    lyric_opts: &LyricProcessOptions,
    write_opts: &WriteOptionsConfig,
    working_has_cover: bool,
) -> (TagPatch, Option<CoverData>, bool) {
    let mut patch = TagPatch::default();
    let set_non_empty = |enabled: bool, val: &str| -> FieldUpdate<String> {
        if enabled && !val.trim().is_empty() {
            FieldUpdate::Set(val.trim().to_string())
        } else {
            FieldUpdate::Keep
        }
    };

    patch.title = set_non_empty(flags.title, &cand.title);
    patch.artist = set_non_empty(flags.artist, &cand.artist);
    patch.album = set_non_empty(flags.album, &cand.album);
    patch.album_artist = if flags.album_artist {
        normalize_optional_album_artist(&cand.album_artist, &cand.artist)
            .map_or(FieldUpdate::Keep, FieldUpdate::Set)
    } else {
        FieldUpdate::Keep
    };
    patch.year = if flags.year {
        normalize_optional_positive_num(&cand.year).map_or(FieldUpdate::Keep, FieldUpdate::Set)
    } else {
        FieldUpdate::Keep
    };
    patch.track = if flags.track {
        normalize_optional_positive_num(&cand.track).map_or(FieldUpdate::Keep, FieldUpdate::Set)
    } else {
        FieldUpdate::Keep
    };
    patch.disc = if flags.disc {
        normalize_optional_disc(&cand.disc).map_or(FieldUpdate::Keep, FieldUpdate::Set)
    } else {
        FieldUpdate::Keep
    };
    patch.genre = set_non_empty(flags.genre, &cand.genre);
    patch.composer = set_non_empty(flags.title && flags.artist, &cand.composer);
    patch.lyricist = set_non_empty(flags.title && flags.artist, &cand.lyricist);

    if flags.comment {
        if flags.write_163_key {
            if let Some(ref meta) = cand.netease_meta {
                if let Ok(encoded) = encode_163_key(meta) {
                    patch.comment = FieldUpdate::Set(encoded);
                }
            } else if !cand.comment.trim().is_empty() {
                patch.comment = FieldUpdate::Set(cand.comment.trim().to_string());
            }
        } else if !cand.comment.trim().is_empty() {
            patch.comment = FieldUpdate::Set(cand.comment.trim().to_string());
        }
    }

    let mut inst_skipped = false;
    if flags.lyrics {
        match process_netease_lyrics(
            cand.raw_lrc.as_deref(),
            cand.raw_tlyric.as_deref(),
            cand.nolyric,
            &cand.title,
            lyric_opts,
        ) {
            ProcessedLyrics::Lyrics(lrc) => {
                patch.lyrics = FieldUpdate::Set(lrc);
            }
            ProcessedLyrics::InstrumentalSkipped => {
                inst_skipped = true;
            }
            ProcessedLyrics::Empty => {}
        }
    }

    let mut cover_preview = None;
    if flags.cover && (write_opts.overwrite_cover || !working_has_cover) {
        if let Some(ref raw_bytes) = cand.cover_bytes {
            let raw_spec = CoverWriteSpec {
                bytes: raw_bytes.clone(),
                mime: String::new(),
                target_width: None,
                target_height: None,
                keep_aspect: true,
                max_kb: (write_opts.pic_size_limit_kb > 0)
                    .then_some(write_opts.pic_size_limit_kb),
                format: write_opts.pic_format,
                overwrite: write_opts.overwrite_cover,
            };
            if let Ok((final_bytes, mime)) = process_cover_image(
                &raw_spec,
                write_opts.pic_res_limit,
                write_opts.pic_size_limit_kb,
                write_opts.pic_format,
            ) {
                let mime_str = mime.as_str().to_string();
                if let Ok(cd) = build_cover_data_from_bytes(final_bytes.clone(), &mime_str) {
                    cover_preview = Some(cd);
                    patch.cover = FieldUpdate::Set(CoverWriteSpec {
                        bytes: final_bytes,
                        mime: mime_str,
                        target_width: None,
                        target_height: None,
                        keep_aspect: true,
                        max_kb: None,
                        format: PicFormatLimit::Auto,
                        overwrite: write_opts.overwrite_cover,
                    });
                }
            }
        }
    }

    (patch, cover_preview, inst_skipped)
}

enum AutoMatchTrackOutcome {
    Matched {
        cand: CandidateTrack,
        patch: TagPatch,
        cover_data: Option<CoverData>,
        instrumental_skipped: bool,
    },
    NotFound(String),
}

fn auto_match_single_track(
    track: &AudioTrack,
    sources: &[Arc<dyn TagSource>],
    cfg: &AppConfig,
    flags: &ComboOverwriteFlags,
    lyric_opts: &LyricProcessOptions,
    write_opts: &WriteOptionsConfig,
) -> AutoMatchTrackOutcome {
    let title = if cfg.search_cond_title_mode == 1 || track.title.trim().is_empty() {
        extract_filename_stem(&track.filename)
    } else {
        track.title.clone()
    };
    let query = SearchQuery {
        title,
        artist: if cfg.search_use_artist {
            track.artist.clone()
        } else {
            String::new()
        },
        album: if cfg.search_use_album {
            track.album.clone()
        } else {
            String::new()
        },
        song_id: if track.netease_id.trim().is_empty() {
            None
        } else {
            Some(track.netease_id.trim().to_string())
        },
        limit: cfg.search_limit.max(1).min(10),
    };

    let mut last_err = None;
    for src in sources {
        let candidates_res = if let Some(ref sid) = query.song_id {
            if src.info().capabilities.key163 || src.info().id == "netease" {
                src.fetch_detail(sid).map(|d| vec![d]).or_else(|_| src.search_combo(&query))
            } else {
                src.search_combo(&query)
            }
        } else {
            src.search_combo(&query)
        };

        let mut list = match candidates_res {
            Ok(l) => l,
            Err(e) => {
                last_err = Some(format!("{}: {e}", src.info().name));
                continue;
            }
        };

        if list.is_empty() {
            continue;
        }

        let best_idx = pick_best_candidate_index(track, &list);
        let mut best = list.swap_remove(best_idx);

        if !best.song_id.is_empty() && (best.raw_lrc.is_none() || best.track.is_empty()) {
            if let Ok(detail) = src.fetch_detail(&best.song_id) {
                merge_candidate_detail(&mut best, detail);
            }
        }
        if flags.lyrics && best.raw_lrc.is_none() && src.info().capabilities.lyrics {
            if let Ok(lrc) = src.fetch_lyrics(&best.song_id) {
                best.nolyric = lrc.nolyric;
                best.raw_lrc = lrc.raw_lrc;
                best.raw_tlyric = lrc.raw_tlyric;
                best.has_translation = best
                    .raw_tlyric
                    .as_deref()
                    .is_some_and(|t| !t.trim().is_empty());
            }
        }
        if flags.cover
            && best.cover_bytes.is_none()
            && (write_opts.overwrite_cover || track.cover.is_none())
        {
            if let Some(ref url) = best.cover_url {
                if let Ok(bytes) = src.download_bytes(url) {
                    best.cover_bytes = Some(bytes);
                }
            }
        }

        let (patch, cover_data, instrumental_skipped) = build_combo_patch(
            &best,
            flags,
            lyric_opts,
            write_opts,
            track.cover.is_some(),
        );
        return AutoMatchTrackOutcome::Matched {
            cand: best,
            patch,
            cover_data,
            instrumental_skipped,
        };
    }

    AutoMatchTrackOutcome::NotFound(
        last_err.unwrap_or_else(|| "无精确匹配候选，请手动组合搜索".to_string()),
    )
}

fn pick_best_candidate_index(track: &AudioTrack, candidates: &[CandidateTrack]) -> usize {
    let target_title = if track.title.trim().is_empty() {
        extract_filename_stem(&track.filename)
    } else {
        track.title.trim().to_string()
    }
    .to_lowercase();
    let target_artist = track.artist.trim().to_lowercase();

    candidates
        .iter()
        .enumerate()
        .max_by_key(|(_, cand)| {
            let mut score = 0i64;
            let ct = cand.title.trim().to_lowercase();
            let ca = cand.artist.trim().to_lowercase();
            if ct == target_title && !target_title.is_empty() {
                score += 100;
            } else if !target_title.is_empty() && (ct.contains(&target_title) || target_title.contains(&ct)) {
                score += 40;
            }
            if !target_artist.is_empty() && (ca == target_artist || ca.contains(&target_artist)) {
                score += 60;
            }
            if track.duration_ms > 0 && cand.duration_ms > 0 {
                let diff = track.duration_ms.abs_diff(cand.duration_ms);
                if diff < 3_000 {
                    score += 30;
                } else if diff < 10_000 {
                    score += 10;
                }
            }
            score
        })
        .map(|(i, _)| i)
        .unwrap_or(0)
}

pub fn resolve_lrc_target_path(track: &AudioTrack, cfg: &AppConfig) -> PathBuf {
    let dir = if cfg.lrc_dir.trim().is_empty() {
        track
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(&track.filedir))
    } else {
        PathBuf::from(cfg.lrc_dir.trim())
    };

    let raw_stem = match cfg.lrc_naming {
        1 if !track.artist.trim().is_empty() && !track.title.trim().is_empty() => {
            sanitize_filename(&format!("{} - {}", track.artist.trim(), track.title.trim()))
        }
        2 if !track.title.trim().is_empty() && !track.artist.trim().is_empty() => {
            sanitize_filename(&format!("{} - {}", track.title.trim(), track.artist.trim()))
        }
        _ => extract_filename_stem(&track.filename),
    };
    let stem = if raw_stem.is_empty() {
        extract_filename_stem(&track.filename)
    } else {
        raw_stem
    };
    dir.join(format!("{stem}.lrc"))
}

pub fn encode_lrc_bytes(text: &str, encoding: &str) -> Vec<u8> {
    match encoding.trim() {
        "UTF-8 BOM" => {
            let mut out = Vec::with_capacity(3 + text.len());
            out.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
            out.extend_from_slice(text.as_bytes());
            out
        }
        "GBK" => GBK.encode(text).0.into_owned(),
        "UTF-16 LE" => {
            let mut out = Vec::with_capacity(2 + text.len() * 2);
            out.extend_from_slice(&[0xFF, 0xFE]);
            for u in text.encode_utf16() {
                out.extend_from_slice(&u.to_le_bytes());
            }
            out
        }
        _ => text.as_bytes().to_vec(),
    }
}

pub fn decode_lrc_bytes(bytes: &[u8], encoding_hint: &str) -> String {
    if bytes.starts_with(&[0xFF, 0xFE]) {
        let mut units = Vec::with_capacity((bytes.len() - 2) / 2);
        for chunk in bytes[2..].chunks_exact(2) {
            units.push(u16::from_le_bytes([chunk[0], chunk[1]]));
        }
        return String::from_utf16_lossy(&units);
    }
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(&bytes[3..]).into_owned();
    }
    if encoding_hint.eq_ignore_ascii_case("GBK") {
        if let Ok(utf8) = std::str::from_utf8(bytes) {
            return utf8.to_string();
        }
        return GBK.decode(bytes).0.into_owned();
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => GBK.decode(bytes).0.into_owned(),
    }
}

fn export_lrc_for_track(track: &AudioTrack, lyrics: &str, cfg: &AppConfig) -> Result<PathBuf> {
    let target = resolve_lrc_target_path(track, cfg);
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create LRC output dir {}", parent.display()))?;
    }
    let bytes = encode_lrc_bytes(lyrics, &cfg.lrc_encoding);
    std::fs::write(&target, bytes)
        .with_context(|| format!("failed to write LRC file {}", target.display()))?;
    Ok(target)
}

fn open_directory_in_file_manager(dir: &Path) -> Result<()> {
    #[cfg(target_os = "linux")]
    let mut cmd = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(dir);
        c
    };
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = std::process::Command::new("open");
        c.arg(dir);
        c
    };
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = std::process::Command::new("explorer");
        c.arg(dir);
        c
    };
    cmd.spawn()
        .with_context(|| format!("failed to launch file manager for {}", dir.display()))?;
    Ok(())
}

fn first_lyric_line(lyrics: &str) -> String {
    lyrics
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

fn format_ms_mmss(ms: u64) -> String {
    if ms == 0 {
        return "--:--".to_string();
    }
    let total_secs = ms / 1000;
    format!("{:02}:{:02}", total_secs / 60, total_secs % 60)
}

fn format_size_bytes(bytes: usize) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.2} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{} KB", (bytes + 1023) / 1024)
    }
}

fn mime_to_label(mime: &str) -> &'static str {
    if mime.eq_ignore_ascii_case("image/png") {
        "PNG"
    } else if mime.eq_ignore_ascii_case("image/webp") {
        "WEBP"
    } else if mime.eq_ignore_ascii_case("image/gif") {
        "GIF"
    } else if mime.eq_ignore_ascii_case("image/bmp") {
        "BMP"
    } else {
        "JPEG"
    }
}

fn mime_to_ext(mime: &str) -> &'static str {
    if mime.eq_ignore_ascii_case("image/png") {
        "png"
    } else if mime.eq_ignore_ascii_case("image/webp") {
        "webp"
    } else if mime.eq_ignore_ascii_case("image/gif") {
        "gif"
    } else if mime.eq_ignore_ascii_case("image/bmp") {
        "bmp"
    } else {
        "jpg"
    }
}

fn format_patch_summary(patch: &TagPatch) -> String {
    let mut parts = Vec::new();
    let mut push_f = |label: &str, upd: &FieldUpdate<String>| {
        if let FieldUpdate::Set(v) = upd {
            parts.push(format!("{label}: {v}"));
        }
    };
    push_f("标题", &patch.title);
    push_f("艺术家", &patch.artist);
    push_f("专辑", &patch.album);
    push_f("专辑艺术家", &patch.album_artist);
    push_f("音轨", &patch.track);
    push_f("碟号", &patch.disc);
    push_f("年份", &patch.year);
    push_f("注释", &patch.comment);
    parts.join(" · ")
}

fn count_occurrences(haystack: &str, needle: &str, match_case: bool) -> usize {
    if needle.is_empty() {
        return 0;
    }
    if match_case {
        haystack.matches(needle).count()
    } else {
        haystack
            .to_lowercase()
            .matches(&needle.to_lowercase())
            .count()
    }
}

fn contains_match(haystack: &str, needle: &str, match_case: bool) -> bool {
    if needle.is_empty() {
        return false;
    }
    if match_case {
        haystack.contains(needle)
    } else {
        haystack.to_lowercase().contains(&needle.to_lowercase())
    }
}

fn track_matches_query(track: &AudioTrack, find: &str, match_case: bool) -> bool {
    [
        track.title.as_str(),
        track.artist.as_str(),
        track.album.as_str(),
        track.album_artist.as_str(),
        track.genre.as_str(),
        track.composer.as_str(),
        track.lyricist.as_str(),
        track.comment.as_str(),
        track.lyrics.as_str(),
    ]
    .into_iter()
    .any(|s| contains_match(s, find, match_case))
}

fn replace_in_track(
    patch: &mut TagPatch,
    current: &AudioTrack,
    find: &str,
    replace: &str,
    match_case: bool,
) -> bool {
    let mut changed = false;
    let mut apply_one = |val: &str, slot: &mut FieldUpdate<String>| {
        if contains_match(val, find, match_case) {
            let next = replace_str(val, find, replace, match_case);
            if next != val {
                *slot = FieldUpdate::Set(next);
                changed = true;
            }
        }
    };
    apply_one(&current.title, &mut patch.title);
    apply_one(&current.artist, &mut patch.artist);
    apply_one(&current.album, &mut patch.album);
    apply_one(&current.album_artist, &mut patch.album_artist);
    apply_one(&current.genre, &mut patch.genre);
    apply_one(&current.composer, &mut patch.composer);
    apply_one(&current.lyricist, &mut patch.lyricist);
    if !current.has_163_key {
        apply_one(&current.comment, &mut patch.comment);
    }
    apply_one(&current.lyrics, &mut patch.lyrics);
    changed
}

fn replace_first_str(input: &str, find: &str, replace: &str, match_case: bool) -> String {
    if find.is_empty() {
        return input.to_string();
    }
    if match_case {
        if let Some(pos) = input.find(find) {
            let mut out = String::with_capacity(input.len() + replace.len());
            out.push_str(&input[..pos]);
            out.push_str(replace);
            out.push_str(&input[pos + find.len()..]);
            return out;
        }
        return input.to_string();
    }
    let lower_in = input.to_lowercase();
    let lower_find = find.to_lowercase();
    if lower_in.len() == input.len() && lower_find.len() == find.len() {
        if let Some(pos) = lower_in.find(&lower_find) {
            let end = pos + find.len();
            if input.is_char_boundary(pos) && input.is_char_boundary(end) {
                let mut out = String::with_capacity(input.len() + replace.len());
                out.push_str(&input[..pos]);
                out.push_str(replace);
                out.push_str(&input[end..]);
                return out;
            }
        }
    }
    input.replacen(find, replace, 1)
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

#[allow(dead_code)]
fn current_timestamp() -> String {
    Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use musictag::core::key163::NetEase163Payload;
    use tempfile::tempdir;

    #[test]
    fn collect_audio_files_filters_exts_video_and_subdirs() {
        let dir = tempdir().unwrap();
        let sub = dir.path().join("nested");
        std::fs::create_dir_all(&sub).unwrap();

        let flac_top = dir.path().join("a.flac");
        let mp4_top = dir.path().join("video.mp4");
        let txt_top = dir.path().join("readme.txt");
        let mp3_sub = sub.join("b.mp3");

        std::fs::write(&flac_top, b"").unwrap();
        std::fs::write(&mp4_top, b"").unwrap();
        std::fs::write(&txt_top, b"").unwrap();
        std::fs::write(&mp3_sub, b"").unwrap();

        let fmt_reg = build_default_format_registry();
        let allowed = AppConfig::default().allowed_extensions();

        let (shallow, counts_shallow) = collect_audio_files(
            &[DirConfigEntry {
                path: dir.path().to_string_lossy().into_owned(),
                include_subdirs: false,
            }],
            &[],
            &fmt_reg,
            &allowed,
            true,
        );
        assert_eq!(shallow, vec![flac_top.clone()]);
        assert_eq!(counts_shallow, vec![1]);

        let (deep_with_mp4, counts_deep) = collect_audio_files(
            &[DirConfigEntry {
                path: dir.path().to_string_lossy().into_owned(),
                include_subdirs: true,
            }],
            &[],
            &fmt_reg,
            &allowed,
            false,
        );
        assert_eq!(deep_with_mp4.len(), 3);
        assert_eq!(counts_deep, vec![3]);
    }

    #[test]
    fn source_registry_sync_and_reorder_with_quickjs_plugin() {
        let plugin_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("plugins")
            .join("sources");
        let mut reg = build_default_source_registry(&[plugin_dir], ";");
        let mut cfg = AppConfig::default();
        sync_registry_with_config(&mut reg, &cfg);

        let list = reg.list();
        assert_eq!(list[0].id, "netease");
        assert!(list[0].enabled);
        let example = list.iter().find(|s| s.id == "example-custom-js").unwrap();
        assert!(!example.enabled);

        // Enable example-custom-js and move it to #1
        cfg.source_priority = vec!["example-custom-js".to_string(), "netease".to_string()];
        cfg.enabled_sources = vec!["example-custom-js".to_string(), "netease".to_string()];
        sync_registry_with_config(&mut reg, &cfg);
        let reordered = reg.list();
        assert_eq!(reordered[0].id, "example-custom-js");
        assert!(reordered[0].enabled);
        assert_eq!(reordered[1].id, "netease");
        assert!(reordered[1].enabled);
    }

    #[test]
    fn patch_merge_preview_and_lrc_encoding_roundtrip() {
        let disk = AudioTrack {
            path: PathBuf::from("/tmp/artist - song.flac"),
            filename: "artist - song.flac".to_string(),
            filedir: "/tmp".to_string(),
            title: "Old Title".to_string(),
            artist: "Old Artist".to_string(),
            ..Default::default()
        };

        let mut p1 = TagPatch {
            title: FieldUpdate::Set("New Title".to_string()),
            album: FieldUpdate::Clear,
            ..Default::default()
        };
        let p2 = TagPatch {
            artist: FieldUpdate::Set("New Artist".to_string()),
            ..Default::default()
        };
        merge_tag_patch(&mut p1, &p2);

        let preview = apply_patch_to_track(&disk, &p1, None);
        assert_eq!(preview.title, "New Title");
        assert_eq!(preview.artist, "New Artist");
        assert_eq!(preview.album, "");

        let sample_lrc = "[00:01.00]中文歌词测试";
        for enc in ["UTF-8", "UTF-8 BOM", "GBK", "UTF-16 LE"] {
            let encoded = encode_lrc_bytes(sample_lrc, enc);
            let decoded = decode_lrc_bytes(&encoded, enc);
            assert_eq!(decoded, sample_lrc, "failed roundtrip for {enc}");
        }
    }

    #[test]
    fn combo_patch_honors_overwrites_163_key_and_instrumental_skip() {
        let mut cand = CandidateTrack {
            source_id: "netease".to_string(),
            source_name: "网易云音乐".to_string(),
            song_id: "1357953768".to_string(),
            title: "Track Title".to_string(),
            artist: "Artist A".to_string(),
            album: "Album A".to_string(),
            raw_lrc: Some("[00:00.00]纯音乐，请欣赏".to_string()),
            netease_meta: Some(NetEase163Payload {
                music_id: 1357953768,
                music_name: "Track Title".to_string(),
                ..Default::default()
            }),
            ..Default::default()
        };

        let flags = ComboOverwriteFlags {
            title: true,
            artist: true,
            album: false,
            album_artist: true,
            year: true,
            track: true,
            disc: true,
            genre: true,
            comment: true,
            lyrics: true,
            cover: false,
            write_163_key: true,
        };
        let lyric_opts = LyricProcessOptions::default();
        let write_opts = WriteOptionsConfig::default();

        let (patch, _cover, inst_skipped) =
            build_combo_patch(&cand, &flags, &lyric_opts, &write_opts, false);
        assert_eq!(patch.title, FieldUpdate::Set("Track Title".to_string()));
        assert_eq!(patch.album, FieldUpdate::Keep);
        assert!(patch.lyrics.is_keep());
        assert!(inst_skipped);
        if let FieldUpdate::Set(c) = &patch.comment {
            assert!(c.starts_with(KEY_163_PREFIX));
            assert_eq!(extract_163_song_id(c).as_deref(), Some("1357953768"));
        } else {
            panic!("expected 163 key comment");
        }

        // Now test with normal vocal lyrics
        cand.raw_lrc = Some("[00:01.00]Vocal line".to_string());
        let (patch2, _cover2, inst2) =
            build_combo_patch(&cand, &flags, &lyric_opts, &write_opts, false);
        assert!(!inst2);
        assert_eq!(patch2.lyrics, FieldUpdate::Set("[00:01.00]Vocal line".to_string()));
    }

    #[test]
    fn combo_patch_omits_missing_and_default_disc_and_album_artist() {
        let flags = ComboOverwriteFlags {
            title: true,
            artist: true,
            album: true,
            album_artist: true,
            year: true,
            track: true,
            disc: true,
            genre: true,
            comment: true,
            lyrics: true,
            cover: false,
            write_163_key: false,
        };
        let lyric_opts = LyricProcessOptions::default();
        let write_opts = WriteOptionsConfig::default();

        for (disc, album_artist, track, year) in [
            ("01", "", "0", "0"),
            ("1", "artist a", "00", "0000"),
            ("1/1", "  ", "", ""),
        ] {
            let cand = CandidateTrack {
                title: "Song".to_string(),
                artist: "Artist A".to_string(),
                album: "Album".to_string(),
                album_artist: album_artist.to_string(),
                disc: disc.to_string(),
                track: track.to_string(),
                year: year.to_string(),
                ..Default::default()
            };
            let (patch, _, _) = build_combo_patch(&cand, &flags, &lyric_opts, &write_opts, false);
            assert_eq!(patch.disc, FieldUpdate::Keep, "disc={disc:?}");
            assert_eq!(
                patch.album_artist,
                FieldUpdate::Keep,
                "album_artist={album_artist:?}"
            );
            assert_eq!(patch.track, FieldUpdate::Keep, "track={track:?}");
            assert_eq!(patch.year, FieldUpdate::Keep, "year={year:?}");
        }

        let multi = CandidateTrack {
            title: "Song".to_string(),
            artist: "Artist A".to_string(),
            album: "Compilation".to_string(),
            album_artist: "Various Artists".to_string(),
            disc: "02".to_string(),
            track: "5".to_string(),
            year: "2020".to_string(),
            ..Default::default()
        };
        let (patch_multi, _, _) =
            build_combo_patch(&multi, &flags, &lyric_opts, &write_opts, false);
        assert_eq!(patch_multi.disc, FieldUpdate::Set("2".to_string()));
        assert_eq!(
            patch_multi.album_artist,
            FieldUpdate::Set("Various Artists".to_string())
        );
        assert_eq!(patch_multi.track, FieldUpdate::Set("5".to_string()));
        assert_eq!(patch_multi.year, FieldUpdate::Set("2020".to_string()));

        let multi_slash = CandidateTrack {
            disc: "2/2".to_string(),
            ..multi
        };
        let (patch_slash, _, _) =
            build_combo_patch(&multi_slash, &flags, &lyric_opts, &write_opts, false);
        assert_eq!(patch_slash.disc, FieldUpdate::Set("2/2".to_string()));
    }
}
