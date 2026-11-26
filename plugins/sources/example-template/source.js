/**
 * MusicTag Native — QuickJS 自定义音源插件模板
 *
 * 运行时宿主会在全局注入：
 * 1. `globalThis.__host`（底层同步原生函数）：
 *    - `httpGet(url, headersJson)` -> JSON 字符串 `{"ok":bool,"status":number,"body":string,"error":string}`
 *    - `httpPost(url, body, headersJson)` -> 同上 JSON 字符串
 *    - `base64Encode(text)` / `base64Decode(b64)`
 *    - `aes128EcbEncrypt(plainUtf8, keyUtf8)` / `aes128EcbDecrypt(b64, keyUtf8)`
 *
 * 2. `globalThis.MusicTagHost`（对 `__host` 的便捷封装，自动 JSON.parse HTTP 响应）：
 *    - `MusicTagHost.httpGet(url, headersObjOrJson)` -> `{ ok, status, body, error }`
 *    - `MusicTagHost.httpPost(url, bodyObjOrStr, headersObjOrJson)` -> `{ ok, status, body, error }`
 *    - `MusicTagHost.base64Encode / base64Decode / aes128EcbEncrypt / aes128EcbDecrypt`
 *
 * 插件需导出（全局定义）以下函数，接收并返回 JSON 字符串（或普通 JS 对象/数组）：
 * - `searchCombo(queryJson)`   -> `CandidateTrack[]`
 * - `fetchDetail(songId)`      -> `CandidateTrack`
 * - `fetchLyrics(songId)`      -> `LyricData`
 * - `searchCover(queryJson)`   -> `CoverSearchItem[]`
 */

function buildKeyword(query) {
    return [query.title, query.artist, query.album]
        .map(function (s) { return (s || "").trim(); })
        .filter(function (s) { return s.length > 0; })
        .join(" ");
}

/**
 * 组合标签搜索
 * @param {string} queryJson - `{"title":string,"artist":string,"album":string,"song_id":string|null,"limit":number}`
 * @returns {string} JSON 格式的 `CandidateTrack[]`
 */
function searchCombo(queryJson) {
    const query = JSON.parse(queryJson);
    const keyword = buildKeyword(query);
    if (!keyword) {
        return "[]";
    }

    const limit = query.limit && query.limit > 0 ? query.limit : 10;
    const url = "https://itunes.apple.com/search?media=music&entity=song&limit="
        + encodeURIComponent(String(limit))
        + "&term=" + encodeURIComponent(keyword);

    const resp = MusicTagHost.httpGet(url, {
        "Accept": "application/json"
    });
    if (!resp.ok) {
        // 离线或请求失败时安全返回空数组，也可 throw new Error(resp.error) 向上汇报错误
        return "[]";
    }

    const data = JSON.parse(resp.body || "{}");
    const results = Array.isArray(data.results) ? data.results : [];

    const candidates = results.map(function (item) {
        const hiResCover = item.artworkUrl100
            ? String(item.artworkUrl100).replace("100x100bb", "1400x1400bb")
            : null;
        const releaseYear = item.releaseDate ? String(item.releaseDate).slice(0, 4) : "";

        return {
            song_id: String(item.trackId || ""),
            title: item.trackName || "",
            artist: item.artistName || "",
            album: item.collectionName || "",
            album_artist: item.artistName || "",
            year: releaseYear,
            track: item.trackNumber ? String(item.trackNumber) : "",
            disc: item.discNumber ? String(item.discNumber) : "",
            genre: item.primaryGenreName || "",
            duration_ms: Number(item.trackTimeMillis || 0),
            cover_url: hiResCover,
            cover_size: hiResCover ? "1400×1400" : "",
            has_translation: false,
            is_instrumental: false,
            lyric_preview: ""
        };
    });

    return JSON.stringify(candidates);
}

/**
 * 获取单曲完整元数据
 * @param {string} songId
 * @returns {string} JSON 格式的 `CandidateTrack`
 */
function fetchDetail(songId) {
    const url = "https://itunes.apple.com/lookup?id=" + encodeURIComponent(String(songId));
    const resp = MusicTagHost.httpGet(url, { "Accept": "application/json" });
    if (!resp.ok) {
        throw new Error("fetchDetail failed: " + resp.error);
    }

    const data = JSON.parse(resp.body || "{}");
    const item = Array.isArray(data.results) ? data.results[0] : null;
    if (!item) {
        throw new Error("Track not found for id: " + songId);
    }

    const hiResCover = item.artworkUrl100
        ? String(item.artworkUrl100).replace("100x100bb", "3000x3000bb")
        : null;

    return JSON.stringify({
        song_id: String(item.trackId || songId),
        title: item.trackName || "",
        artist: item.artistName || "",
        album: item.collectionName || "",
        album_artist: item.artistName || "",
        year: item.releaseDate ? String(item.releaseDate).slice(0, 4) : "",
        track: item.trackNumber ? String(item.trackNumber) : "",
        disc: item.discNumber ? String(item.discNumber) : "",
        genre: item.primaryGenreName || "",
        duration_ms: Number(item.trackTimeMillis || 0),
        cover_url: hiResCover,
        cover_size: hiResCover ? "3000×3000" : ""
    });
}

/**
 * 获取歌词数据
 * @param {string} songId
 * @returns {string} JSON 格式的 `LyricData`
 */
function fetchLyrics(songId) {
    // 示例返回空歌词占位；实际插件可调用歌词接口并填充 raw_lrc / raw_tlyric
    return JSON.stringify({
        raw_lrc: null,
        raw_tlyric: null,
        nolyric: false,
        uncollected: true
    });
}

/**
 * 搜索高分辨率封面候选
 * @param {string} queryJson
 * @returns {string} JSON 格式的 `CoverSearchItem[]`
 */
function searchCover(queryJson) {
    const candidates = JSON.parse(searchCombo(queryJson));
    const covers = [];
    const seen = {};

    for (let i = 0; i < candidates.length; i++) {
        const c = candidates[i];
        if (!c.cover_url || seen[c.cover_url]) {
            continue;
        }
        seen[c.cover_url] = true;
        covers.push({
            title: c.album || c.title,
            url: c.cover_url,
            width: 1400,
            height: 1400,
            format: "JPEG"
        });
    }

    return JSON.stringify(covers);
}
