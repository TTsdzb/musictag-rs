use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;

use crate::core::models::{
    AudioTrack, CandidateTrack, CoverSearchItem, LyricData, ReplayGainData, RgScanMode,
    SearchQuery, SourceInfo, TagPatch, WriteOptionsConfig,
};

/// Online metadata, lyrics, and cover art source abstraction.
pub trait TagSource: Send + Sync {
    fn info(&self) -> SourceInfo;

    fn search_combo(&self, query: &SearchQuery) -> Result<Vec<CandidateTrack>>;

    fn fetch_detail(&self, song_id: &str) -> Result<CandidateTrack>;

    fn fetch_lyrics(&self, song_id: &str) -> Result<LyricData>;

    fn search_cover(&self, query: &SearchQuery) -> Result<Vec<CoverSearchItem>>;

    fn download_bytes(&self, url: &str) -> Result<Vec<u8>>;
}

/// Audio container/tag format reader and writer abstraction.
pub trait FormatHandler: Send + Sync {
    fn id(&self) -> &'static str;

    fn extensions(&self) -> &'static [&'static str];

    fn read(&self, path: &Path) -> Result<AudioTrack>;

    fn write_patch(
        &self,
        path: &Path,
        patch: &TagPatch,
        opts: &WriteOptionsConfig,
    ) -> Result<()>;

    fn write_replaygain(
        &self,
        path: &Path,
        rg: &ReplayGainData,
        opts: &WriteOptionsConfig,
    ) -> Result<()>;
}

/// Progress callback invoked during ReplayGain batch scanning: `(completed, total, current_path)`.
pub type ScanProgressCallback<'a> = &'a (dyn Fn(usize, usize, &Path) + Send + Sync);

/// ReplayGain loudness/peak scanner abstraction.
pub trait ReplayGainScanner: Send + Sync {
    fn scan_tracks(
        &self,
        paths: &[PathBuf],
        mode: RgScanMode,
        target_lufs: f64,
        clip_prevent: bool,
    ) -> Result<Vec<(PathBuf, ReplayGainData)>>;

    fn scan_tracks_with_progress(
        &self,
        paths: &[PathBuf],
        mode: RgScanMode,
        target_lufs: f64,
        clip_prevent: bool,
        on_progress: Option<ScanProgressCallback<'_>>,
    ) -> Result<Vec<(PathBuf, ReplayGainData)>> {
        let results = self.scan_tracks(paths, mode, target_lufs, clip_prevent)?;
        if let Some(cb) = on_progress {
            let total = paths.len();
            for (idx, (path, _)) in results.iter().enumerate() {
                cb(idx + 1, total, path);
            }
        }
        Ok(results)
    }
}

struct RegisteredSource {
    source: Arc<dyn TagSource>,
    enabled: bool,
}

/// Ordered registry of metadata, lyrics, and cover sources.
#[derive(Default)]
pub struct SourceRegistry {
    entries: Vec<RegisteredSource>,
}

impl SourceRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, source: Arc<dyn TagSource>) {
        let info = source.info();
        if let Some(existing) = self.entries.iter_mut().find(|e| e.source.info().id == info.id) {
            existing.source = source;
            existing.enabled = info.enabled;
        } else {
            self.entries.push(RegisteredSource {
                source,
                enabled: info.enabled,
            });
        }
    }

    pub fn list(&self) -> Vec<SourceInfo> {
        self.entries
            .iter()
            .map(|entry| {
                let mut info = entry.source.info();
                info.enabled = entry.enabled;
                info
            })
            .collect()
    }

    pub fn reorder(&mut self, ordered_ids: &[String]) {
        let mut by_id: HashMap<String, RegisteredSource> = self
            .entries
            .drain(..)
            .map(|e| (e.source.info().id, e))
            .collect();

        for id in ordered_ids {
            if let Some(entry) = by_id.remove(id) {
                self.entries.push(entry);
            }
        }

        let mut remaining: Vec<RegisteredSource> = by_id.into_values().collect();
        remaining.sort_by(|a, b| a.source.info().id.cmp(&b.source.info().id));
        self.entries.extend(remaining);
    }

    pub fn set_enabled(&mut self, id: &str, enabled: bool) -> bool {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.source.info().id == id) {
            entry.enabled = enabled;
            true
        } else {
            false
        }
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn TagSource>> {
        self.entries
            .iter()
            .find(|e| e.source.info().id == id)
            .map(|e| Arc::clone(&e.source))
    }

    pub fn active_combo_sources(&self) -> Vec<Arc<dyn TagSource>> {
        self.entries
            .iter()
            .filter(|e| e.enabled && e.source.info().capabilities.combo)
            .map(|e| Arc::clone(&e.source))
            .collect()
    }

    pub fn active_lyrics_sources(&self) -> Vec<Arc<dyn TagSource>> {
        self.entries
            .iter()
            .filter(|e| e.enabled && e.source.info().capabilities.lyrics)
            .map(|e| Arc::clone(&e.source))
            .collect()
    }

    pub fn active_cover_sources(&self) -> Vec<Arc<dyn TagSource>> {
        self.entries
            .iter()
            .filter(|e| e.enabled && e.source.info().capabilities.cover)
            .map(|e| Arc::clone(&e.source))
            .collect()
    }

    pub fn active_combo(&self) -> Option<Arc<dyn TagSource>> {
        self.active_combo_sources().into_iter().next()
    }

    pub fn active_lyrics(&self) -> Option<Arc<dyn TagSource>> {
        self.active_lyrics_sources().into_iter().next()
    }

    pub fn active_cover(&self) -> Option<Arc<dyn TagSource>> {
        self.active_cover_sources().into_iter().next()
    }
}

/// Registry mapping lowercase file extensions to `FormatHandler` implementations.
#[derive(Default)]
pub struct FormatRegistry {
    handlers: Vec<Arc<dyn FormatHandler>>,
    ext_map: HashMap<String, Arc<dyn FormatHandler>>,
}

impl FormatRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, handler: Arc<dyn FormatHandler>) {
        for &ext in handler.extensions() {
            let norm = normalize_ext(ext);
            self.ext_map.insert(norm, Arc::clone(&handler));
        }
        self.handlers.push(handler);
    }

    pub fn by_extension(&self, ext: &str) -> Option<Arc<dyn FormatHandler>> {
        self.ext_map.get(&normalize_ext(ext)).cloned()
    }

    pub fn by_path(&self, path: &Path) -> Option<Arc<dyn FormatHandler>> {
        let ext = path.extension()?.to_str()?;
        self.by_extension(ext)
    }

    pub fn supported_extensions(&self) -> Vec<String> {
        let mut exts: Vec<String> = self.ext_map.keys().cloned().collect();
        exts.sort();
        exts
    }

    pub fn read(&self, path: &Path) -> Result<AudioTrack> {
        let handler = self.by_path(path).ok_or_else(|| {
            anyhow::anyhow!("unsupported audio format: {}", path.display())
        })?;
        handler.read(path)
    }

    pub fn write_patch(
        &self,
        path: &Path,
        patch: &TagPatch,
        opts: &WriteOptionsConfig,
    ) -> Result<()> {
        let handler = self.by_path(path).ok_or_else(|| {
            anyhow::anyhow!("unsupported audio format: {}", path.display())
        })?;
        handler.write_patch(path, patch, opts)
    }

    pub fn write_replaygain(
        &self,
        path: &Path,
        rg: &ReplayGainData,
        opts: &WriteOptionsConfig,
    ) -> Result<()> {
        let handler = self.by_path(path).ok_or_else(|| {
            anyhow::anyhow!("unsupported audio format: {}", path.display())
        })?;
        handler.write_replaygain(path, rg, opts)
    }
}

fn normalize_ext(ext: &str) -> String {
    ext.trim().trim_start_matches('.').to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::models::SourceCapabilities;

    struct DummySource {
        info: SourceInfo,
    }

    impl TagSource for DummySource {
        fn info(&self) -> SourceInfo {
            self.info.clone()
        }

        fn search_combo(&self, _query: &SearchQuery) -> Result<Vec<CandidateTrack>> {
            Ok(Vec::new())
        }

        fn fetch_detail(&self, _song_id: &str) -> Result<CandidateTrack> {
            anyhow::bail!("unused")
        }

        fn fetch_lyrics(&self, _song_id: &str) -> Result<LyricData> {
            Ok(LyricData::default())
        }

        fn search_cover(&self, _query: &SearchQuery) -> Result<Vec<CoverSearchItem>> {
            Ok(Vec::new())
        }

        fn download_bytes(&self, _url: &str) -> Result<Vec<u8>> {
            Ok(Vec::new())
        }
    }

    struct DummyFormat;

    impl FormatHandler for DummyFormat {
        fn id(&self) -> &'static str {
            "dummy-flac"
        }

        fn extensions(&self) -> &'static [&'static str] {
            &["flac", ".FLAC"]
        }

        fn read(&self, path: &Path) -> Result<AudioTrack> {
            Ok(AudioTrack {
                path: path.to_path_buf(),
                ext: "flac".to_string(),
                ..Default::default()
            })
        }

        fn write_patch(
            &self,
            _path: &Path,
            _patch: &TagPatch,
            _opts: &WriteOptionsConfig,
        ) -> Result<()> {
            Ok(())
        }

        fn write_replaygain(
            &self,
            _path: &Path,
            _rg: &ReplayGainData,
            _opts: &WriteOptionsConfig,
        ) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn source_registry_orders_enables_and_looks_up() {
        let mut reg = SourceRegistry::new();
        reg.register(Arc::new(DummySource {
            info: SourceInfo {
                id: "netease".to_string(),
                name: "网易云".to_string(),
                enabled: true,
                capabilities: SourceCapabilities {
                    combo: true,
                    lyrics: true,
                    cover: true,
                    key163: true,
                },
                ..Default::default()
            },
        }));
        reg.register(Arc::new(DummySource {
            info: SourceInfo {
                id: "musicbrainz".to_string(),
                name: "MusicBrainz".to_string(),
                enabled: false,
                capabilities: SourceCapabilities {
                    combo: true,
                    lyrics: false,
                    cover: true,
                    key163: false,
                },
                ..Default::default()
            },
        }));

        assert_eq!(reg.active_combo().unwrap().info().id, "netease");
        assert_eq!(reg.active_lyrics().unwrap().info().id, "netease");
        assert_eq!(reg.active_cover().unwrap().info().id, "netease");

        assert!(reg.set_enabled("musicbrainz", true));
        reg.reorder(&["musicbrainz".to_string(), "netease".to_string()]);

        assert_eq!(reg.active_combo().unwrap().info().id, "musicbrainz");
        assert_eq!(reg.active_lyrics().unwrap().info().id, "netease");
        assert_eq!(reg.list()[0].id, "musicbrainz");
        assert!(reg.get("netease").is_some());
    }

    #[test]
    fn format_registry_resolves_extensions_and_dispatches() {
        let mut reg = FormatRegistry::new();
        reg.register(Arc::new(DummyFormat));

        assert!(reg.by_extension(".FLAC").is_some());
        assert!(reg.by_path(Path::new("/tmp/song.FlAc")).is_some());
        assert!(reg.by_path(Path::new("/tmp/song.wav")).is_none());

        let track = reg.read(Path::new("/tmp/song.flac")).unwrap();
        assert_eq!(track.ext, "flac");
        assert!(reg
            .write_patch(
                Path::new("/tmp/song.flac"),
                &TagPatch::default(),
                &WriteOptionsConfig::default()
            )
            .is_ok());
        assert!(reg.read(Path::new("/tmp/unknown.xyz")).is_err());
    }
}
