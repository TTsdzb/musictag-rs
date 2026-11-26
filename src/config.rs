use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Local;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::core::models::{
    AudioTrack, LyricProcessOptions, PicFormatLimit, RgScanMode, WriteOptionsConfig, ZhConvMode,
};

pub const DEFAULT_FILE_EXTS: &str =
    ".aac;.aif;.aifc;.aiff;.ape;.dff;.dsf;.flac;.m4a;.mp3;.mp4;.mpc;.ogg;.opus;.tak;.wav;.wma;.wv;";
const APP_DIR_NAME: &str = "musictag-native";
const CONFIG_FILE_NAME: &str = "config.toml";
const HISTORY_DB_NAME: &str = "history.db";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirConfigEntry {
    pub path: String,
    pub include_subdirs: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnConfigEntry {
    pub key: String,
    pub visible: bool,
    pub width: i32,
}

/// Persistent TOML application configuration aligned with `ui/state.slint` options.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    // Tag sources & formatting
    pub write_163_key: bool,
    pub artist_sep: String,
    pub id3v2_version: u8,
    pub source_priority: Vec<String>,
    pub enabled_sources: Vec<String>,
    pub search_cond_title_mode: u8,
    pub search_use_artist: bool,
    pub search_use_album: bool,
    pub search_limit: usize,

    // Lyrics download & processing
    pub dl_trans: bool,
    pub dont_dl_orig: bool,
    pub trans_format: u8,
    pub trans_connector: String,
    pub delete_head_tag: bool,
    pub delete_blank_lines: bool,
    pub reformat_timetag: bool,
    pub remove_timetag: bool,
    pub skip_instrumental: bool,
    pub zh_conv: u8,

    // ReplayGain
    pub rg_mode: u8,
    pub rg_target_lufs: f64,
    pub rg_clip_prevent: bool,
    pub rg_lowercase_flac: bool,

    // Misc 1 (LRC export, Cover limits, File filtering)
    pub save_lrc_with_tag: bool,
    pub lrc_dir: String,
    pub lrc_naming: u8,
    pub lrc_encoding: String,
    pub keep_mtime: bool,
    pub pic_res_limit: u32,
    pub pic_size_limit_kb: u32,
    pub pic_format: u8,
    pub overwrite_cover: bool,
    pub ignore_video: bool,
    pub duration_filter: u8,
    pub file_exts: String,

    // Misc 2 (Appearance, Threads, Tray, Directories, Table Columns)
    pub thread_count: usize,
    pub minimize_tray: bool,
    pub always_tray: bool,
    pub check_updates: bool,
    pub theme_preference: i32,
    pub working_dir: String,
    pub dirs: Vec<DirConfigEntry>,
    pub columns: Vec<ColumnConfigEntry>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            write_163_key: true,
            artist_sep: ";".to_string(),
            id3v2_version: 4,
            source_priority: vec![
                "netease".to_string(),
                "musicbrainz".to_string(),
                "qq".to_string(),
                "kugou".to_string(),
            ],
            enabled_sources: vec!["netease".to_string()],
            search_cond_title_mode: 0,
            search_use_artist: true,
            search_use_album: true,
            search_limit: 10,

            dl_trans: true,
            dont_dl_orig: false,
            trans_format: 0,
            trans_connector: "\\n".to_string(),
            delete_head_tag: true,
            delete_blank_lines: true,
            reformat_timetag: true,
            remove_timetag: false,
            skip_instrumental: true,
            zh_conv: 0,

            rg_mode: 0,
            rg_target_lufs: -18.0,
            rg_clip_prevent: true,
            rg_lowercase_flac: true,

            save_lrc_with_tag: false,
            lrc_dir: String::new(),
            lrc_naming: 0,
            lrc_encoding: "UTF-8".to_string(),
            keep_mtime: false,
            pic_res_limit: 0,
            pic_size_limit_kb: 2000,
            pic_format: 0,
            overwrite_cover: true,
            ignore_video: true,
            duration_filter: 0,
            file_exts: DEFAULT_FILE_EXTS.to_string(),

            thread_count: 4,
            minimize_tray: false,
            always_tray: false,
            check_updates: true,
            theme_preference: 2,
            working_dir: String::new(),
            dirs: Vec::new(),
            columns: Vec::new(),
        }
    }
}

impl AppConfig {
    /// Returns the default config file path (`<config_dir>/musictag-native/config.toml`).
    pub fn default_path() -> Result<PathBuf> {
        let base = dirs::config_dir().context("could not locate user config directory")?;
        Ok(base.join(APP_DIR_NAME).join(CONFIG_FILE_NAME))
    }

    /// Loads config from `Self::default_path()`, returning `Default` if the file does not exist.
    pub fn load_default() -> Result<Self> {
        let path = Self::default_path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        Self::load_from(&path)
    }

    /// Saves config to `Self::default_path()`, creating parent directories if needed.
    pub fn save_default(&self) -> Result<PathBuf> {
        let path = Self::default_path()?;
        self.save_to(&path)?;
        Ok(path)
    }

    /// Loads TOML configuration from `path` (falling back to defaults if `path` does not exist).
    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config file {}", path.display()))?;
        toml::from_str::<Self>(&raw)
            .with_context(|| format!("failed to parse TOML config {}", path.display()))
    }

    /// Writes configuration as pretty TOML to `path`, creating parent directories automatically.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("failed to create config directory {}", parent.display())
            })?;
        }
        let serialized =
            toml::to_string_pretty(self).context("failed to serialize AppConfig to TOML")?;
        std::fs::write(path, serialized)
            .with_context(|| format!("failed to write config file {}", path.display()))
    }

    pub fn id3v2_ver_index(&self) -> i32 {
        if self.id3v2_version <= 3 { 0 } else { 1 }
    }

    pub fn set_id3v2_ver_index(&mut self, index: i32) {
        self.id3v2_version = if index == 0 { 3 } else { 4 };
    }

    pub fn to_write_options(&self) -> WriteOptionsConfig {
        WriteOptionsConfig {
            id3v2_version: if self.id3v2_version <= 3 { 3 } else { 4 },
            keep_mtime: self.keep_mtime,
            overwrite_cover: self.overwrite_cover,
            pic_res_limit: self.pic_res_limit,
            pic_size_limit_kb: self.pic_size_limit_kb,
            pic_format: PicFormatLimit::from_index(self.pic_format as i32),
            rg_lowercase_flac: self.rg_lowercase_flac,
        }
    }

    pub fn to_lyric_options(&self) -> LyricProcessOptions {
        LyricProcessOptions {
            dl_trans: self.dl_trans,
            dont_dl_orig: self.dont_dl_orig,
            trans_format: self.trans_format,
            trans_connector: self.trans_connector.clone(),
            delete_head_tag: self.delete_head_tag,
            delete_blank_lines: self.delete_blank_lines,
            reformat_timetag: self.reformat_timetag,
            remove_timetag: self.remove_timetag,
            skip_instrumental: self.skip_instrumental,
            zh_conv: ZhConvMode::from_index(self.zh_conv as i32),
        }
    }

    pub fn to_rg_scan_mode(&self) -> RgScanMode {
        RgScanMode::from_index(self.rg_mode as i32)
    }

    pub fn allowed_extensions(&self) -> Vec<String> {
        self.file_exts
            .split([';', ',', ' '])
            .map(|s| s.trim().trim_start_matches('.').to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect()
    }
}

/// Input payload used to record a tag snapshot in `HistoryStore`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TagSnapshotInput {
    pub track_path: String,
    pub timestamp: Option<String>,
    pub summary: String,
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
    pub lyrics: String,
    pub comment_has_163: bool,
}

/// Persisted tag snapshot returned from `HistoryStore`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagHistoryRecord {
    pub id: i64,
    pub track_path: String,
    pub timestamp: String,
    pub summary: String,
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
    pub lyrics: String,
    pub comment_has_163: bool,
}

/// SQLite-backed store of tag modification snapshots.
pub struct HistoryStore {
    conn: Connection,
}

impl HistoryStore {
    /// Returns the default database path (`<data_local_dir>/musictag-native/history.db`).
    pub fn default_path() -> Result<PathBuf> {
        let base =
            dirs::data_local_dir().context("could not locate user local data directory")?;
        Ok(base.join(APP_DIR_NAME).join(HISTORY_DB_NAME))
    }

    pub fn open_default() -> Result<Self> {
        let path = Self::default_path()?;
        Self::open_at(&path)
    }

    pub fn open_at(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("failed to create history database dir {}", parent.display())
            })?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("failed to open SQLite history db {}", path.display()))?;
        Self::from_connection(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("failed to open in-memory SQLite db")?;
        Self::from_connection(conn)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS tag_history (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                track_path TEXT NOT NULL,
                timestamp TEXT NOT NULL,
                summary TEXT NOT NULL,
                title TEXT NOT NULL,
                artist TEXT NOT NULL,
                album TEXT NOT NULL,
                album_artist TEXT NOT NULL,
                year TEXT NOT NULL,
                track_num TEXT NOT NULL,
                disc_num TEXT NOT NULL,
                genre TEXT NOT NULL,
                composer TEXT NOT NULL,
                lyricist TEXT NOT NULL,
                comment TEXT NOT NULL,
                lyrics TEXT NOT NULL,
                comment_has_163 INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_tag_history_path ON tag_history(track_path);",
        )
        .context("failed to initialize tag_history schema")?;
        Ok(Self { conn })
    }

    pub fn record(&self, snapshot: &TagSnapshotInput) -> Result<i64> {
        let ts = snapshot
            .timestamp
            .clone()
            .unwrap_or_else(|| Local::now().format("%Y-%m-%d %H:%M:%S").to_string());
        self.conn.execute(
            "INSERT INTO tag_history (
                track_path, timestamp, summary, title, artist, album, album_artist,
                year, track_num, disc_num, genre, composer, lyricist, comment,
                lyrics, comment_has_163
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                snapshot.track_path,
                ts,
                snapshot.summary,
                snapshot.title,
                snapshot.artist,
                snapshot.album,
                snapshot.album_artist,
                snapshot.year,
                snapshot.track,
                snapshot.disc,
                snapshot.genre,
                snapshot.composer,
                snapshot.lyricist,
                snapshot.comment,
                snapshot.lyrics,
                i32::from(snapshot.comment_has_163),
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn record_track(&self, track: &AudioTrack, summary: &str) -> Result<i64> {
        let input = TagSnapshotInput {
            track_path: track.path.to_string_lossy().into_owned(),
            timestamp: None,
            summary: summary.to_string(),
            title: track.title.clone(),
            artist: track.artist.clone(),
            album: track.album.clone(),
            album_artist: track.album_artist.clone(),
            year: track.year.clone(),
            track: track.track.clone(),
            disc: track.disc.clone(),
            genre: track.genre.clone(),
            composer: track.composer.clone(),
            lyricist: track.lyricist.clone(),
            comment: track.comment.clone(),
            lyrics: track.lyrics.clone(),
            comment_has_163: track.has_163_key,
        };
        self.record(&input)
    }

    pub fn list(&self, limit: usize) -> Result<Vec<TagHistoryRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, track_path, timestamp, summary, title, artist, album, album_artist,
                    year, track_num, disc_num, genre, composer, lyricist, comment,
                    lyrics, comment_has_163
             FROM tag_history
             ORDER BY id DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], row_to_record)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn list_for_track(&self, track_path: &str, limit: usize) -> Result<Vec<TagHistoryRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, track_path, timestamp, summary, title, artist, album, album_artist,
                    year, track_num, disc_num, genre, composer, lyricist, comment,
                    lyrics, comment_has_163
             FROM tag_history
             WHERE track_path = ?1
             ORDER BY id DESC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![track_path, limit as i64], row_to_record)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn get(&self, id: i64) -> Result<Option<TagHistoryRecord>> {
        self.conn
            .query_row(
                "SELECT id, track_path, timestamp, summary, title, artist, album, album_artist,
                        year, track_num, disc_num, genre, composer, lyricist, comment,
                        lyrics, comment_has_163
                 FROM tag_history
                 WHERE id = ?1",
                params![id],
                row_to_record,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn clear(&self) -> Result<usize> {
        let deleted = self.conn.execute("DELETE FROM tag_history", [])?;
        Ok(deleted)
    }

    pub fn count(&self) -> Result<usize> {
        let cnt: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM tag_history", [], |row| row.get(0))?;
        Ok(cnt.max(0) as usize)
    }
}

fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<TagHistoryRecord> {
    Ok(TagHistoryRecord {
        id: row.get(0)?,
        track_path: row.get(1)?,
        timestamp: row.get(2)?,
        summary: row.get(3)?,
        title: row.get(4)?,
        artist: row.get(5)?,
        album: row.get(6)?,
        album_artist: row.get(7)?,
        year: row.get(8)?,
        track: row.get(9)?,
        disc: row.get(10)?,
        genre: row.get(11)?,
        composer: row.get(12)?,
        lyricist: row.get(13)?,
        comment: row.get(14)?,
        lyrics: row.get(15)?,
        comment_has_163: row.get::<_, i32>(16)? != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn app_config_defaults_and_toml_roundtrip() {
        let cfg = AppConfig::default();
        assert!(cfg.rg_lowercase_flac);
        assert_eq!(cfg.artist_sep, ";");
        assert_eq!(cfg.id3v2_version, 4);
        assert!(cfg.write_163_key);
        assert!(cfg.dl_trans);
        assert_eq!(cfg.trans_format, 0);
        assert!(cfg.skip_instrumental);
        assert!(cfg.delete_head_tag);
        assert!(cfg.delete_blank_lines);
        assert!(cfg.reformat_timetag);
        assert_eq!(cfg.rg_target_lufs, -18.0);
        assert_eq!(cfg.theme_preference, 2);

        let dir = tempdir().unwrap();
        let path = dir.path().join("nested/config.toml");

        let mut modified = cfg.clone();
        modified.artist_sep = " / ".to_string();
        modified.id3v2_version = 3;
        modified.theme_preference = 1;
        modified.dirs.push(DirConfigEntry {
            path: "/music".to_string(),
            include_subdirs: true,
        });
        modified.save_to(&path).unwrap();

        let loaded = AppConfig::load_from(&path).unwrap();
        assert_eq!(loaded, modified);

        // Partial TOML uses defaults for missing keys
        let partial_path = dir.path().join("partial.toml");
        std::fs::write(&partial_path, "write_163_key = false\n").unwrap();
        let partial = AppConfig::load_from(&partial_path).unwrap();
        assert!(!partial.write_163_key);
        assert!(partial.rg_lowercase_flac);
        assert_eq!(partial.rg_target_lufs, -18.0);
        assert_eq!(partial.theme_preference, 2);
    }

    #[test]
    fn history_store_crud_in_memory_and_file() {
        let store = HistoryStore::open_in_memory().unwrap();
        assert_eq!(store.count().unwrap(), 0);

        let id1 = store
            .record(&TagSnapshotInput {
                track_path: "/music/a.flac".to_string(),
                timestamp: Some("2026-11-24 10:00:00".to_string()),
                summary: "Initial read".to_string(),
                title: "Song A".to_string(),
                artist: "Artist A".to_string(),
                album: "Album A".to_string(),
                comment_has_163: false,
                ..Default::default()
            })
            .unwrap();

        let id2 = store
            .record(&TagSnapshotInput {
                track_path: "/music/a.flac".to_string(),
                timestamp: Some("2026-11-24 10:05:00".to_string()),
                summary: "NetEase match".to_string(),
                title: "Song A (Remastered)".to_string(),
                artist: "Artist A".to_string(),
                album: "Album A".to_string(),
                comment: "163 key(Don't modify):...".to_string(),
                comment_has_163: true,
                ..Default::default()
            })
            .unwrap();

        assert_eq!(store.count().unwrap(), 2);
        let list = store.list(10).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, id2);
        assert_eq!(list[1].id, id1);

        let got = store.get(id1).unwrap().unwrap();
        assert_eq!(got.title, "Song A");
        assert!(!got.comment_has_163);

        let per_track = store.list_for_track("/music/a.flac", 10).unwrap();
        assert_eq!(per_track.len(), 2);

        let cleared = store.clear().unwrap();
        assert_eq!(cleared, 2);
        assert_eq!(store.count().unwrap(), 0);

        // File-backed check
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("sub/history.db");
        let file_store = HistoryStore::open_at(&db_path).unwrap();
        file_store
            .record_track(
                &AudioTrack {
                    path: PathBuf::from("/music/b.flac"),
                    title: "B".to_string(),
                    has_163_key: true,
                    ..Default::default()
                },
                "Saved",
            )
            .unwrap();
        drop(file_store);

        let reopened = HistoryStore::open_at(&db_path).unwrap();
        assert_eq!(reopened.count().unwrap(), 1);
    }
}
