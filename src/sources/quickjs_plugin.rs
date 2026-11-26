use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use aes::Aes128;
use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use anyhow::{Context as AnyhowContext, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use reqwest::blocking::Client;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, USER_AGENT};
use rquickjs::function::{Func, Opt};
use rquickjs::{CatchResultExt, CaughtError, Context, Ctx, Exception, Function, Object, Runtime, Value};
use serde::{Deserialize, Serialize};

use crate::core::key163::NetEase163Payload;
use crate::core::models::{
    CandidateTrack, CoverSearchItem, LyricData, SearchQuery, SourceCapabilities, SourceInfo,
};
use crate::core::traits::TagSource;

const PLUGIN_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36";
const JS_MEMORY_LIMIT_BYTES: usize = 64 * 1024 * 1024;
const JS_MAX_STACK_BYTES: usize = 1024 * 1024;

const MUSIC_TAG_HOST_BOOTSTRAP: &str = r#"
(function () {
    const rawHost = globalThis.__host;
    if (!rawHost) {
        throw new Error("MusicTag __host bindings missing");
    }
    globalThis.MusicTagHost = {
        httpGet(url, headers) {
            const hdr = headers == null
                ? ""
                : (typeof headers === "string" ? headers : JSON.stringify(headers));
            return JSON.parse(rawHost.httpGet(String(url), hdr));
        },
        httpPost(url, body, headers) {
            const payload = body == null
                ? ""
                : (typeof body === "string" ? body : JSON.stringify(body));
            const hdr = headers == null
                ? ""
                : (typeof headers === "string" ? headers : JSON.stringify(headers));
            return JSON.parse(rawHost.httpPost(String(url), payload, hdr));
        },
        httpGetRaw(url, headersJson) {
            return rawHost.httpGet(
                String(url),
                headersJson == null ? "" : String(headersJson)
            );
        },
        httpPostRaw(url, body, headersJson) {
            return rawHost.httpPost(
                String(url),
                body == null ? "" : String(body),
                headersJson == null ? "" : String(headersJson)
            );
        },
        base64Encode(text) {
            return rawHost.base64Encode(String(text));
        },
        base64Decode(b64) {
            return rawHost.base64Decode(String(b64));
        },
        aes128EcbEncrypt(plainUtf8, keyUtf8) {
            return rawHost.aes128EcbEncrypt(String(plainUtf8), String(keyUtf8));
        },
        aes128EcbDecrypt(b64, keyUtf8) {
            return rawHost.aes128EcbDecrypt(String(b64), String(keyUtf8));
        }
    };
    if (typeof globalThis.console === "undefined") {
        globalThis.console = {
            log: function () {},
            info: function () {},
            warn: function () {},
            error: function () {}
        };
    }
})();
"#;

/// TOML manifest (`plugin.toml`) describing a QuickJS source plugin.
#[derive(Debug, Clone, Deserialize)]
pub struct PluginManifest {
    pub id: String,
    pub name: String,
    #[serde(default = "default_version")]
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_entry")]
    pub entry: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub capabilities: PluginCapabilitiesManifest,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct PluginCapabilitiesManifest {
    pub combo: bool,
    pub lyrics: bool,
    pub cover: bool,
    pub key163: bool,
}

impl Default for PluginCapabilitiesManifest {
    fn default() -> Self {
        Self {
            combo: true,
            lyrics: true,
            cover: true,
            key163: false,
        }
    }
}

impl From<PluginCapabilitiesManifest> for SourceCapabilities {
    fn from(value: PluginCapabilitiesManifest) -> Self {
        Self {
            combo: value.combo,
            lyrics: value.lyrics,
            cover: value.cover,
            key163: value.key163,
        }
    }
}

fn default_version() -> String {
    "1.0.0".to_string()
}

fn default_entry() -> String {
    "source.js".to_string()
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Serialize)]
struct HostHttpResponse {
    ok: bool,
    status: u16,
    body: String,
    error: String,
}

enum WorkerCommand {
    Call {
        fn_name: &'static str,
        arg: String,
        required: bool,
        reply: mpsc::Sender<Result<Option<String>>>,
    },
}

/// Sandboxed QuickJS `TagSource` plugin running on a dedicated JS worker thread.
pub struct QuickJsSourcePlugin {
    info: SourceInfo,
    plugin_dir: PathBuf,
    client: Arc<Client>,
    sender: Mutex<Option<mpsc::Sender<WorkerCommand>>>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for QuickJsSourcePlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuickJsSourcePlugin")
            .field("info", &self.info)
            .field("plugin_dir", &self.plugin_dir)
            .finish()
    }
}

impl QuickJsSourcePlugin {
    /// Loads and validates a QuickJS plugin from a directory containing `plugin.toml`.
    pub fn from_dir(dir: &Path) -> Result<Self> {
        let manifest_path = dir.join("plugin.toml");
        let raw_manifest = std::fs::read_to_string(&manifest_path)
            .with_context(|| format!("failed to read {}", manifest_path.display()))?;
        let manifest: PluginManifest = toml::from_str(&raw_manifest)
            .with_context(|| format!("failed to parse {}", manifest_path.display()))?;

        let id = manifest.id.trim().to_string();
        if id.is_empty() {
            bail!("plugin manifest {} has empty `id`", manifest_path.display());
        }
        let name = if manifest.name.trim().is_empty() {
            id.clone()
        } else {
            manifest.name.trim().to_string()
        };

        let entry_rel = validate_relative_entry(&manifest.entry)?;
        let entry_path = dir.join(&entry_rel);
        let script = std::fs::read_to_string(&entry_path)
            .with_context(|| format!("failed to read plugin script {}", entry_path.display()))?;

        let info = SourceInfo {
            id: id.clone(),
            name,
            version: manifest.version.trim().to_string(),
            description: manifest.description.trim().to_string(),
            builtin: false,
            enabled: manifest.enabled,
            capabilities: manifest.capabilities.into(),
        };

        let client = Arc::new(build_plugin_client()?);
        let worker_client = Arc::clone(&client);
        let (cmd_tx, cmd_rx) = mpsc::channel::<WorkerCommand>();
        let (init_tx, init_rx) = mpsc::sync_channel::<Result<()>>(1);
        let thread_name = format!("qjs-src-{id}");

        let handle = thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                run_plugin_worker(script, worker_client, init_tx, cmd_rx);
            })
            .context("failed to spawn QuickJS plugin worker thread")?;

        match init_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                info,
                plugin_dir: dir.to_path_buf(),
                client,
                sender: Mutex::new(Some(cmd_tx)),
                worker: Mutex::new(Some(handle)),
            }),
            Ok(Err(err)) => {
                drop(cmd_tx);
                let _ = handle.join();
                Err(err).with_context(|| {
                    format!("failed to initialize QuickJS plugin '{}'", info.id)
                })
            }
            Err(err) => {
                drop(cmd_tx);
                let _ = handle.join();
                Err(anyhow!("QuickJS plugin '{}' worker aborted during init: {err}", info.id))
            }
        }
    }

    pub fn plugin_dir(&self) -> &Path {
        &self.plugin_dir
    }

    fn call_js(&self, fn_name: &'static str, arg: String, required: bool) -> Result<Option<String>> {
        let tx = {
            let guard = self
                .sender
                .lock()
                .map_err(|_| anyhow!("QuickJS plugin '{}' sender mutex poisoned", self.info.id))?;
            guard
                .as_ref()
                .cloned()
                .ok_or_else(|| anyhow!("QuickJS plugin '{}' worker shut down", self.info.id))?
        };

        let (reply_tx, reply_rx) = mpsc::channel();
        tx.send(WorkerCommand::Call {
            fn_name,
            arg,
            required,
            reply: reply_tx,
        })
        .map_err(|_| anyhow!("QuickJS plugin '{}' worker channel closed", self.info.id))?;

        reply_rx
            .recv()
            .map_err(|_| anyhow!("QuickJS plugin '{}' worker dropped reply", self.info.id))?
    }
}

impl Drop for QuickJsSourcePlugin {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.sender.lock() {
            guard.take();
        }
        if let Ok(mut guard) = self.worker.lock() {
            if let Some(handle) = guard.take() {
                let _ = handle.join();
            }
        }
    }
}

impl TagSource for QuickJsSourcePlugin {
    fn info(&self) -> SourceInfo {
        self.info.clone()
    }

    fn search_combo(&self, query: &SearchQuery) -> Result<Vec<CandidateTrack>> {
        if !self.info.capabilities.combo {
            return Ok(Vec::new());
        }
        let query_json =
            serde_json::to_string(query).context("failed to serialize SearchQuery")?;
        let Some(raw) = self.call_js("searchCombo", query_json, true)? else {
            return Ok(Vec::new());
        };
        let dtos: Vec<PluginCandidateTrack> = serde_json::from_str(&raw).with_context(|| {
            format!(
                "plugin '{}' searchCombo returned invalid CandidateTrack[] JSON",
                self.info.id
            )
        })?;
        Ok(dtos
            .into_iter()
            .map(|dto| dto.into_candidate(&self.info))
            .collect())
    }

    fn fetch_detail(&self, song_id: &str) -> Result<CandidateTrack> {
        let raw = self
            .call_js("fetchDetail", song_id.to_string(), true)?
            .ok_or_else(|| {
                anyhow!("plugin '{}' fetchDetail returned empty result", self.info.id)
            })?;
        let dto: PluginCandidateTrack = serde_json::from_str(&raw).with_context(|| {
            format!(
                "plugin '{}' fetchDetail returned invalid CandidateTrack JSON",
                self.info.id
            )
        })?;
        Ok(dto.into_candidate(&self.info))
    }

    fn fetch_lyrics(&self, song_id: &str) -> Result<LyricData> {
        if !self.info.capabilities.lyrics {
            return Ok(LyricData::default());
        }
        let Some(raw) = self.call_js("fetchLyrics", song_id.to_string(), true)? else {
            return Ok(LyricData::default());
        };
        let dto: PluginLyricData = serde_json::from_str(&raw).with_context(|| {
            format!(
                "plugin '{}' fetchLyrics returned invalid LyricData JSON",
                self.info.id
            )
        })?;
        Ok(dto.into())
    }

    fn search_cover(&self, query: &SearchQuery) -> Result<Vec<CoverSearchItem>> {
        if !self.info.capabilities.cover {
            return Ok(Vec::new());
        }
        let query_json =
            serde_json::to_string(query).context("failed to serialize SearchQuery")?;
        let Some(raw) = self.call_js("searchCover", query_json, true)? else {
            return Ok(Vec::new());
        };
        let dtos: Vec<PluginCoverSearchItem> = serde_json::from_str(&raw).with_context(|| {
            format!(
                "plugin '{}' searchCover returned invalid CoverSearchItem[] JSON",
                self.info.id
            )
        })?;
        Ok(dtos
            .into_iter()
            .map(|dto| dto.into_cover_item(&self.info))
            .collect())
    }

    fn download_bytes(&self, url: &str) -> Result<Vec<u8>> {
        let trimmed = url.trim();
        if trimmed.is_empty() {
            bail!("cannot download bytes from empty URL");
        }
        let resp = self
            .client
            .get(trimmed)
            .send()
            .with_context(|| format!("plugin '{}' failed to download {trimmed}", self.info.id))?
            .error_for_status()
            .with_context(|| format!("plugin '{}' HTTP error downloading {trimmed}", self.info.id))?;
        let bytes = resp
            .bytes()
            .with_context(|| format!("plugin '{}' failed reading bytes from {trimmed}", self.info.id))?;
        Ok(bytes.to_vec())
    }
}

/// Scans `dirs` for QuickJS plugin directories containing `plugin.toml`, returning loaded plugins
/// deduplicated by `id` in discovery order.
pub fn discover_quickjs_plugins(dirs: &[PathBuf]) -> Vec<QuickJsSourcePlugin> {
    let mut plugins = Vec::new();
    let mut seen_ids = HashSet::new();

    for dir in dirs {
        if !dir.exists() || !dir.is_dir() {
            continue;
        }

        let mut candidate_dirs = Vec::new();
        if dir.join("plugin.toml").is_file() {
            candidate_dirs.push(dir.clone());
        } else if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() && path.join("plugin.toml").is_file() {
                    candidate_dirs.push(path);
                }
            }
            candidate_dirs.sort();
        }

        for plugin_dir in candidate_dirs {
            match QuickJsSourcePlugin::from_dir(&plugin_dir) {
                Ok(plugin) => {
                    let id = plugin.info().id;
                    if seen_ids.insert(id) {
                        plugins.push(plugin);
                    }
                }
                Err(err) => {
                    eprintln!(
                        "Skipping QuickJS plugin at {}: {err:#}",
                        plugin_dir.display()
                    );
                }
            }
        }
    }

    plugins
}

fn validate_relative_entry(entry: &str) -> Result<PathBuf> {
    let trimmed = entry.trim();
    if trimmed.is_empty() {
        bail!("plugin `entry` path must not be empty");
    }
    let path = Path::new(trimmed);
    if path.is_absolute() {
        bail!("plugin `entry` must be a relative path: {trimmed}");
    }
    for comp in path.components() {
        if matches!(comp, Component::ParentDir | Component::RootDir | Component::Prefix(_)) {
            bail!("plugin `entry` escapes plugin directory: {trimmed}");
        }
    }
    Ok(path.to_path_buf())
}

fn build_plugin_client() -> Result<Client> {
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static(PLUGIN_USER_AGENT));
    Client::builder()
        .timeout(Duration::from_secs(15))
        .default_headers(headers)
        .build()
        .context("failed to build plugin reqwest blocking client")
}

fn run_plugin_worker(
    script: String,
    client: Arc<Client>,
    init_tx: mpsc::SyncSender<Result<()>>,
    cmd_rx: mpsc::Receiver<WorkerCommand>,
) {
    let setup = (|| -> Result<(Runtime, Context)> {
        let rt = Runtime::new().context("failed to create QuickJS runtime")?;
        rt.set_memory_limit(JS_MEMORY_LIMIT_BYTES);
        rt.set_max_stack_size(JS_MAX_STACK_BYTES);
        let ctx = Context::full(&rt).context("failed to create QuickJS context")?;

        ctx.with(|ctx| -> Result<()> {
            install_host_bindings(&ctx, Arc::clone(&client))?;
            ctx.eval::<(), _>(MUSIC_TAG_HOST_BOOTSTRAP)
                .catch(&ctx)
                .map_err(|err| format_caught_error("host bootstrap", err))?;
            ctx.eval::<(), _>(script.as_str())
                .catch(&ctx)
                .map_err(|err| format_caught_error("plugin script", err))?;
            Ok(())
        })?;

        Ok((rt, ctx))
    })();

    let (_rt, ctx) = match setup {
        Ok(pair) => {
            let _ = init_tx.send(Ok(()));
            pair
        }
        Err(err) => {
            let _ = init_tx.send(Err(err));
            return;
        }
    };

    while let Ok(cmd) = cmd_rx.recv() {
        match cmd {
            WorkerCommand::Call {
                fn_name,
                arg,
                required,
                reply,
            } => {
                let res = ctx.with(|ctx| invoke_global_fn(&ctx, fn_name, &arg, required));
                let _ = reply.send(res);
            }
        }
    }
}

fn install_host_bindings<'js>(ctx: &Ctx<'js>, client: Arc<Client>) -> Result<()> {
    let globals = ctx.globals();
    let host = Object::new(ctx.clone()).context("failed to create __host object")?;

    let get_client = Arc::clone(&client);
    host.set(
        "httpGet",
        Func::from(
            move |_ctx: Ctx<'js>, url: String, headers_json: Opt<Option<String>>| {
                let hdrs = headers_json.0.flatten();
                perform_http_request(&get_client, reqwest::Method::GET, &url, None, hdrs.as_deref())
            },
        ),
    )
    .context("failed to register __host.httpGet")?;

    let post_client = Arc::clone(&client);
    host.set(
        "httpPost",
        Func::from(
            move |_ctx: Ctx<'js>,
                  url: String,
                  body: Opt<Option<String>>,
                  headers_json: Opt<Option<String>>| {
                let body_str = body.0.flatten().unwrap_or_default();
                let hdrs = headers_json.0.flatten();
                perform_http_request(
                    &post_client,
                    reqwest::Method::POST,
                    &url,
                    Some(body_str),
                    hdrs.as_deref(),
                )
            },
        ),
    )
    .context("failed to register __host.httpPost")?;

    host.set(
        "base64Encode",
        Func::from(|text: String| STANDARD.encode(text.as_bytes())),
    )
    .context("failed to register __host.base64Encode")?;

    host.set(
        "base64Decode",
        Func::from(|ctx: Ctx<'js>, b64: String| -> rquickjs::Result<String> {
            match decode_base64_utf8(&b64) {
                Ok(s) => Ok(s),
                Err(err) => Err(Exception::throw_message(&ctx, &err.to_string())),
            }
        }),
    )
    .context("failed to register __host.base64Decode")?;

    host.set(
        "aes128EcbEncrypt",
        Func::from(
            |ctx: Ctx<'js>, plain_utf8: String, key_utf8: String| -> rquickjs::Result<String> {
                match aes128_ecb_encrypt_b64(&plain_utf8, &key_utf8) {
                    Ok(s) => Ok(s),
                    Err(err) => Err(Exception::throw_message(&ctx, &err.to_string())),
                }
            },
        ),
    )
    .context("failed to register __host.aes128EcbEncrypt")?;

    host.set(
        "aes128EcbDecrypt",
        Func::from(
            |ctx: Ctx<'js>, b64: String, key_utf8: String| -> rquickjs::Result<String> {
                match aes128_ecb_decrypt_utf8(&b64, &key_utf8) {
                    Ok(s) => Ok(s),
                    Err(err) => Err(Exception::throw_message(&ctx, &err.to_string())),
                }
            },
        ),
    )
    .context("failed to register __host.aes128EcbDecrypt")?;

    globals
        .set("__host", host)
        .context("failed to attach globalThis.__host")?;

    Ok(())
}

fn invoke_global_fn<'js>(
    ctx: &Ctx<'js>,
    fn_name: &str,
    arg: &str,
    required: bool,
) -> Result<Option<String>> {
    let globals = ctx.globals();
    let val: Value<'js> = globals
        .get(fn_name)
        .catch(ctx)
        .map_err(|err| format_caught_error(fn_name, err))?;

    if val.is_undefined() || val.is_null() {
        if required {
            bail!("plugin does not define global function `{fn_name}`");
        }
        return Ok(None);
    }

    let func = Function::from_value(val)
        .map_err(|err| anyhow!("global `{fn_name}` is not a function: {err}"))?;

    let ret: Value<'js> = func
        .call((arg,))
        .catch(ctx)
        .map_err(|err| format_caught_error(fn_name, err))?;

    if ret.is_undefined() || ret.is_null() {
        return Ok(None);
    }
    if let Some(js_str) = ret.as_string() {
        let s = js_str
            .to_string()
            .context("failed to convert JS return string to UTF-8")?;
        return Ok(Some(s));
    }

    let json_opt = ctx
        .json_stringify(ret)
        .catch(ctx)
        .map_err(|err| format_caught_error(fn_name, err))?;
    match json_opt {
        Some(js_str) => Ok(Some(
            js_str
                .to_string()
                .context("failed to convert JSON.stringify result to UTF-8")?,
        )),
        None => Ok(None),
    }
}

fn format_caught_error<'js>(label: &str, err: CaughtError<'js>) -> anyhow::Error {
    match err {
        CaughtError::Exception(ex) => {
            let msg = ex.message().unwrap_or_else(|| "JS exception".to_string());
            match ex.stack() {
                Some(stack) if !stack.trim().is_empty() => {
                    anyhow!("{label} JS error: {msg}\n{stack}")
                }
                _ => anyhow!("{label} JS error: {msg}"),
            }
        }
        CaughtError::Value(v) => anyhow!("{label} threw JS value: {v:?}"),
        CaughtError::Error(e) => anyhow!("{label} QuickJS error: {e}"),
    }
}

fn perform_http_request(
    client: &Client,
    method: reqwest::Method,
    url: &str,
    body: Option<String>,
    headers_json: Option<&str>,
) -> String {
    let response = execute_http_inner(client, method, url, body, headers_json);
    serde_json::to_string(&response).unwrap_or_else(|err| {
        format!(r#"{{"ok":false,"status":0,"body":"","error":"serialize failed: {err}"}}"#)
    })
}

fn execute_http_inner(
    client: &Client,
    method: reqwest::Method,
    url: &str,
    body: Option<String>,
    headers_json: Option<&str>,
) -> HostHttpResponse {
    let trimmed_url = url.trim();
    if trimmed_url.is_empty() {
        return HostHttpResponse {
            ok: false,
            status: 0,
            body: String::new(),
            error: "empty URL".to_string(),
        };
    }

    let mut req = client.request(method, trimmed_url);

    if let Some(hdr_str) = headers_json {
        let trimmed_hdr = hdr_str.trim();
        if !trimmed_hdr.is_empty() && trimmed_hdr != "null" {
            match parse_headers_json(trimmed_hdr) {
                Ok(headers) => {
                    req = req.headers(headers);
                }
                Err(err) => {
                    return HostHttpResponse {
                        ok: false,
                        status: 0,
                        body: String::new(),
                        error: format!("invalid headersJson: {err}"),
                    };
                }
            }
        }
    }

    if let Some(payload) = body {
        req = req.body(payload);
    }

    match req.send() {
        Ok(resp) => {
            let status = resp.status();
            let status_code = status.as_u16();
            let ok = status.is_success();
            match resp.text() {
                Ok(text) => HostHttpResponse {
                    ok,
                    status: status_code,
                    body: text,
                    error: if ok {
                        String::new()
                    } else {
                        format!("HTTP {status_code}")
                    },
                },
                Err(err) => HostHttpResponse {
                    ok: false,
                    status: status_code,
                    body: String::new(),
                    error: format!("failed to read HTTP body: {err}"),
                },
            }
        }
        Err(err) => HostHttpResponse {
            ok: false,
            status: 0,
            body: String::new(),
            error: err.to_string(),
        },
    }
}

fn parse_headers_json(raw: &str) -> Result<HeaderMap> {
    let val: serde_json::Value =
        serde_json::from_str(raw).context("headersJson must be valid JSON object")?;
    let Some(obj) = val.as_object() else {
        bail!("headersJson must be a JSON object");
    };
    let mut map = HeaderMap::new();
    for (k, v) in obj {
        let header_name = HeaderName::from_bytes(k.as_bytes())
            .with_context(|| format!("invalid HTTP header name: {k}"))?;
        let val_str = match v {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::Bool(b) => b.to_string(),
            _ => continue,
        };
        let header_val = HeaderValue::from_str(&val_str)
            .with_context(|| format!("invalid HTTP header value for {k}"))?;
        map.insert(header_name, header_val);
    }
    Ok(map)
}

fn decode_base64_utf8(b64: &str) -> Result<String> {
    let bytes = STANDARD
        .decode(b64.trim())
        .context("invalid base64 input")?;
    String::from_utf8(bytes).context("decoded base64 is not valid UTF-8")
}

fn aes128_ecb_encrypt_b64(plain_utf8: &str, key_utf8: &str) -> Result<String> {
    let key_bytes = key_utf8.as_bytes();
    if key_bytes.len() != 16 {
        bail!(
            "AES-128 key must be exactly 16 bytes, got {} bytes",
            key_bytes.len()
        );
    }
    let mut buf = plain_utf8.as_bytes().to_vec();
    let pad_len = 16 - (buf.len() % 16);
    buf.resize(buf.len() + pad_len, pad_len as u8);

    let cipher = Aes128::new(GenericArray::from_slice(key_bytes));
    for chunk in buf.chunks_exact_mut(16) {
        cipher.encrypt_block(GenericArray::from_mut_slice(chunk));
    }

    Ok(STANDARD.encode(&buf))
}

fn aes128_ecb_decrypt_utf8(b64: &str, key_utf8: &str) -> Result<String> {
    let key_bytes = key_utf8.as_bytes();
    if key_bytes.len() != 16 {
        bail!(
            "AES-128 key must be exactly 16 bytes, got {} bytes",
            key_bytes.len()
        );
    }
    let mut data = STANDARD
        .decode(b64.trim())
        .context("invalid base64 ciphertext")?;
    if data.is_empty() || data.len() % 16 != 0 {
        bail!(
            "invalid AES-128 ECB ciphertext length: {} bytes",
            data.len()
        );
    }

    let cipher = Aes128::new(GenericArray::from_slice(key_bytes));
    for chunk in data.chunks_exact_mut(16) {
        cipher.decrypt_block(GenericArray::from_mut_slice(chunk));
    }

    let pad_len = *data.last().context("empty decrypted buffer")? as usize;
    if pad_len == 0 || pad_len > 16 || pad_len > data.len() {
        bail!("invalid PKCS#7 padding length: {pad_len}");
    }
    if !data[data.len() - pad_len..]
        .iter()
        .all(|&b| b as usize == pad_len)
    {
        bail!("corrupted PKCS#7 padding in decrypted ciphertext");
    }
    data.truncate(data.len() - pad_len);

    String::from_utf8(data).context("decrypted AES-128 ECB payload is not valid UTF-8")
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PluginCandidateTrack {
    #[serde(alias = "sourceId")]
    source_id: String,
    #[serde(alias = "sourceName")]
    source_name: String,
    #[serde(alias = "songId")]
    song_id: String,
    title: String,
    artist: String,
    album: String,
    #[serde(alias = "albumArtist")]
    album_artist: String,
    year: String,
    track: String,
    disc: String,
    genre: String,
    composer: String,
    lyricist: String,
    comment: String,
    #[serde(alias = "durationMs")]
    duration_ms: u64,
    #[serde(alias = "coverUrl")]
    cover_url: Option<String>,
    #[serde(alias = "coverSize")]
    cover_size: String,
    #[serde(alias = "coverBytes")]
    cover_bytes: Option<Vec<u8>>,
    #[serde(alias = "neteaseMeta")]
    netease_meta: Option<NetEase163Payload>,
    #[serde(alias = "rawLrc")]
    raw_lrc: Option<String>,
    #[serde(alias = "rawTlyric")]
    raw_tlyric: Option<String>,
    nolyric: bool,
    #[serde(alias = "isInstrumental")]
    is_instrumental: bool,
    #[serde(alias = "hasTranslation")]
    has_translation: bool,
    #[serde(alias = "lyricPreview")]
    lyric_preview: String,
}

impl PluginCandidateTrack {
    fn into_candidate(self, info: &SourceInfo) -> CandidateTrack {
        let source_id = if self.source_id.trim().is_empty() {
            info.id.clone()
        } else {
            self.source_id
        };
        let source_name = if self.source_name.trim().is_empty() {
            info.name.clone()
        } else {
            self.source_name
        };
        let has_translation = self.has_translation
            || self
                .raw_tlyric
                .as_deref()
                .is_some_and(|s| !s.trim().is_empty());

        CandidateTrack {
            source_id,
            source_name,
            song_id: self.song_id,
            title: self.title,
            artist: self.artist,
            album: self.album,
            album_artist: self.album_artist,
            year: self.year,
            track: self.track,
            disc: self.disc,
            genre: self.genre,
            composer: self.composer,
            lyricist: self.lyricist,
            comment: self.comment,
            duration_ms: self.duration_ms,
            cover_url: self.cover_url.filter(|s| !s.trim().is_empty()),
            cover_size: self.cover_size,
            cover_bytes: self.cover_bytes,
            netease_meta: self.netease_meta,
            raw_lrc: self.raw_lrc,
            raw_tlyric: self.raw_tlyric,
            nolyric: self.nolyric,
            is_instrumental: self.is_instrumental,
            has_translation,
            lyric_preview: self.lyric_preview,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PluginLyricData {
    #[serde(alias = "rawLrc")]
    raw_lrc: Option<String>,
    #[serde(alias = "rawTlyric")]
    raw_tlyric: Option<String>,
    nolyric: bool,
    uncollected: bool,
}

impl From<PluginLyricData> for LyricData {
    fn from(value: PluginLyricData) -> Self {
        Self {
            raw_lrc: value.raw_lrc.filter(|s| !s.trim().is_empty()),
            raw_tlyric: value.raw_tlyric.filter(|s| !s.trim().is_empty()),
            nolyric: value.nolyric,
            uncollected: value.uncollected,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PluginCoverSearchItem {
    #[serde(alias = "sourceId")]
    source_id: String,
    #[serde(alias = "sourceName")]
    source_name: String,
    title: String,
    url: String,
    width: Option<u32>,
    height: Option<u32>,
    #[serde(alias = "sizeBytes")]
    size_bytes: Option<usize>,
    format: String,
    #[serde(alias = "thumbBytes")]
    thumb_bytes: Option<Vec<u8>>,
}

impl PluginCoverSearchItem {
    fn into_cover_item(self, info: &SourceInfo) -> CoverSearchItem {
        let source_id = if self.source_id.trim().is_empty() {
            info.id.clone()
        } else {
            self.source_id
        };
        let source_name = if self.source_name.trim().is_empty() {
            info.name.clone()
        } else {
            self.source_name
        };
        CoverSearchItem {
            source_id,
            source_name,
            title: self.title,
            url: self.url,
            width: self.width,
            height: self.height,
            size_bytes: self.size_bytes,
            format: if self.format.trim().is_empty() {
                "JPEG".to_string()
            } else {
                self.format
            },
            thumb_bytes: self.thumb_bytes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::key163::{NetEase163Payload, decode_163_key};
    use tempfile::tempdir;

    #[test]
    fn test_quickjs_plugin_sandbox_and_crypto() {
        let root = tempdir().expect("create temp plugin root");
        let plugin_dir = root.path().join("sample-js");
        std::fs::create_dir_all(&plugin_dir).unwrap();

        let manifest = r#"
id = "sample-crypto-js"
name = "Sample Crypto JS"
version = "0.1.0"
description = "Offline sandbox & crypto verification plugin"
entry = "source.js"
enabled = true

[capabilities]
combo = true
lyrics = true
cover = true
key163 = false
"#;
        std::fs::write(plugin_dir.join("plugin.toml"), manifest).unwrap();

        let js_source = r###"
const KEY_163 = "#14ljk_!\\]&0U<'(";

function searchCombo(queryJson) {
    const q = JSON.parse(queryJson);
    const b64 = __host.base64Encode(q.title || "untitled");
    const decryptedB64 = __host.base64Decode(b64);

    const payload = JSON.stringify({
        format: "mp3",
        musicId: 1357953768,
        musicName: decryptedB64,
        artist: [["ヨルシカ", 28082342]],
        album: q.album || "だから僕は音楽を辞めた",
        albumId: 78436910,
        albumPic: "https://p1.music.126.net/test/109951163963487312.jpg",
        albumPicDocId: 109951163963487312,
        mvId: 0,
        flag: 4,
        alias: [],
        transNames: [],
        bitrate: 320000,
        duration: 242000
    });
    const enc = __host.aes128EcbEncrypt("music:" + payload, KEY_163);
    const dec = __host.aes128EcbDecrypt(enc, KEY_163);

    return JSON.stringify([
        {
            song_id: "1357953768",
            title: decryptedB64,
            artist: q.artist || "ヨルシカ",
            album: q.album || "だから僕は音楽を辞めた",
            year: "2019",
            track: "14",
            duration_ms: 242000,
            comment: "163 key(Don't modify):" + enc,
            cover_url: "https://p1.music.126.net/test/109951163963487312.jpg",
            lyric_preview: dec
        }
    ]);
}

function fetchDetail(songId) {
    return JSON.stringify({
        song_id: songId,
        title: "だから僕は音楽を辞めた",
        artist: "ヨルシカ",
        album: "だから僕は音楽を辞めた",
        raw_lrc: "[00:01.08]考えたってわからないし",
        raw_tlyric: "[00:01.08]想过之后依然搞不懂",
        has_translation: true
    });
}

function fetchLyrics(songId) {
    return JSON.stringify({
        raw_lrc: "[00:01.08]Line for " + songId,
        raw_tlyric: "[00:01.08]翻译 " + songId,
        nolyric: false,
        uncollected: false
    });
}

function searchCover(queryJson) {
    const q = JSON.parse(queryJson);
    return JSON.stringify([
        {
            title: q.album || q.title || "Cover",
            url: "https://example.invalid/cover.png",
            width: 1500,
            height: 1500,
            format: "PNG"
        }
    ]);
}
"###;
        std::fs::write(plugin_dir.join("source.js"), js_source).unwrap();

        let plugins = discover_quickjs_plugins(&[root.path().to_path_buf()]);
        assert_eq!(plugins.len(), 1);
        let plugin = &plugins[0];
        let info = plugin.info();
        assert_eq!(info.id, "sample-crypto-js");
        assert_eq!(info.name, "Sample Crypto JS");
        assert!(!info.builtin);
        assert!(info.enabled);
        assert!(info.capabilities.combo);
        assert!(!info.capabilities.key163);

        // Verify TagSource is Send + Sync by wrapping in Arc<dyn TagSource> and querying from another thread.
        let arc_source: Arc<dyn TagSource> = Arc::new(plugins.into_iter().next().unwrap());
        let cloned = Arc::clone(&arc_source);
        let candidates = std::thread::spawn(move || {
            cloned.search_combo(&SearchQuery {
                title: "だから僕は音楽を辞めた".to_string(),
                artist: "ヨルシカ".to_string(),
                album: "だから僕は音楽を辞めた".to_string(),
                song_id: None,
                limit: 5,
            })
        })
        .join()
        .expect("worker thread join")
        .expect("search_combo succeeds");

        assert_eq!(candidates.len(), 1);
        let cand = &candidates[0];
        assert_eq!(cand.source_id, "sample-crypto-js");
        assert_eq!(cand.source_name, "Sample Crypto JS");
        assert_eq!(cand.title, "だから僕は音楽を辞めた");
        assert!(cand.comment.starts_with("163 key(Don't modify):"));
        assert!(cand.lyric_preview.starts_with("music:"));

        // Verify JS-encrypted AES-128-ECB payload decodes cleanly via core::key163::decode_163_key!
        let decoded_163: NetEase163Payload =
            decode_163_key(&cand.comment).expect("core key163 decodes JS-encrypted comment");
        assert_eq!(decoded_163.music_id, 1357953768);
        assert_eq!(decoded_163.music_name, "だから僕は音楽を辞めた");

        let detail = arc_source.fetch_detail("1357953768").expect("fetch_detail");
        assert_eq!(detail.song_id, "1357953768");
        assert!(detail.has_translation);

        let lyrics = arc_source.fetch_lyrics("1357953768").expect("fetch_lyrics");
        assert_eq!(lyrics.raw_lrc.as_deref(), Some("[00:01.08]Line for 1357953768"));
        assert_eq!(lyrics.raw_tlyric.as_deref(), Some("[00:01.08]翻译 1357953768"));
        assert!(!lyrics.nolyric);

        let covers = arc_source
            .search_cover(&SearchQuery {
                album: "Elma".to_string(),
                ..Default::default()
            })
            .expect("search_cover");
        assert_eq!(covers.len(), 1);
        assert_eq!(covers[0].title, "Elma");
        assert_eq!(covers[0].format, "PNG");
        assert_eq!(covers[0].width, Some(1500));
    }

    #[test]
    fn quickjs_rejects_path_traversal_and_invalid_js() {
        let root = tempdir().expect("tempdir");
        let bad_path_dir = root.path().join("bad-path");
        std::fs::create_dir_all(&bad_path_dir).unwrap();
        std::fs::write(
            bad_path_dir.join("plugin.toml"),
            "id = \"bad\"\nname = \"Bad\"\nentry = \"../escape.js\"\n",
        )
        .unwrap();
        assert!(QuickJsSourcePlugin::from_dir(&bad_path_dir).is_err());

        let bad_js_dir = root.path().join("bad-js");
        std::fs::create_dir_all(&bad_js_dir).unwrap();
        std::fs::write(
            bad_js_dir.join("plugin.toml"),
            "id = \"bad-js\"\nname = \"BadJS\"\n",
        )
        .unwrap();
        std::fs::write(bad_js_dir.join("source.js"), "function broken( { ").unwrap();
        assert!(QuickJsSourcePlugin::from_dir(&bad_js_dir).is_err());
    }

    #[test]
    fn quickjs_host_http_validates_inputs_and_headers_offline() {
        let headers = parse_headers_json(r#"{"User-Agent":"TestAgent","X-Num":42}"#)
            .expect("valid headers JSON");
        assert_eq!(headers.get("user-agent").unwrap(), "TestAgent");
        assert_eq!(headers.get("x-num").unwrap(), "42");
        assert!(parse_headers_json("not-json").is_err());
        assert!(parse_headers_json("[]").is_err());

        let root = tempdir().unwrap();
        let plugin_dir = root.path().join("http-offline-check");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(
            plugin_dir.join("plugin.toml"),
            "id = \"http-offline\"\nname = \"HTTP Offline Check\"\n",
        )
        .unwrap();

        let js = r#"
function searchCombo(queryJson) {
    const emptyUrlResp = MusicTagHost.httpGet("", null);
    const badHdrResp = MusicTagHost.httpPost("https://example.invalid/api", "body", "not-json");
    const rawEmptyResp = JSON.parse(__host.httpGet("", ""));
    return JSON.stringify([{
        song_id: "1",
        title: String(emptyUrlResp.ok) + ":" + emptyUrlResp.error,
        artist: String(badHdrResp.ok) + ":" + badHdrResp.error,
        comment: String(rawEmptyResp.status) + ":" + rawEmptyResp.error
    }]);
}
"#;
        std::fs::write(plugin_dir.join("source.js"), js).unwrap();

        let plugin = QuickJsSourcePlugin::from_dir(&plugin_dir).expect("load offline check plugin");
        let cands = plugin
            .search_combo(&SearchQuery::default())
            .expect("searchCombo runs cleanly");
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].title, "false:empty URL");
        assert!(cands[0].artist.starts_with("false:invalid headersJson:"));
        assert_eq!(cands[0].comment, "0:empty URL");
        assert!(plugin.download_bytes("   ").is_err());
    }
}
