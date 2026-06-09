//! Language Server Protocol クライアント（`gd`/`gr` の精度向上用）。
//!
//! srev は **読み取り専用** のレビューアなので、編集系同期（didChange/didSave）は不要で、
//! ファイルを開いた時に一度 `textDocument/didOpen` を送るだけで良い。これにより LSP 実装を
//! 大幅に簡略化している。
//!
//! 非同期は既存 [`crate::tags::ProjectIndex`] と同じ **thread + mpsc + 毎フレーム poll** で行う
//! （tokio は使わない）。各言語サーバーは子プロセスとして遅延起動し、`gd`/`gr` 要求は
//! ID で対応付けて応答到着時に解決する。サーバーが無い／未初期化／空応答のときは呼び出し側で
//! tree-sitter-tags にフォールバックする。
//!
//! 位置は LSP 既定の **UTF-16 コードユニット** を使う（srev のカーソル列は char 基準なので
//! [`char_to_utf16`] / [`utf16_to_char`] で相互変換する）。

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::thread;
use std::time::Duration;

use lsp_types::{DocumentSymbol, DocumentSymbolResponse, GotoDefinitionResponse, SymbolKind, Url};
use serde::Deserialize;
use serde_json::{Value, json};

/// 解決済みのコード位置（参照/定義のジャンプ先）。`character` は UTF-16 列。
#[derive(Clone, Debug)]
pub struct Loc {
    pub path: PathBuf,
    pub line: usize,
    pub character: u32,
}

/// char 列 → UTF-16 列（LSP へ送る位置の計算）。
pub fn char_to_utf16(line: &str, char_col: usize) -> u32 {
    line.chars().take(char_col).map(|c| c.len_utf16() as u32).sum()
}

/// UTF-16 列 → char 列（LSP から受けた位置を srev のカーソル列へ）。
pub fn utf16_to_char(line: &str, utf16_col: u32) -> usize {
    let mut units = 0u32;
    for (i, c) in line.chars().enumerate() {
        if units >= utf16_col {
            return i;
        }
        units += c.len_utf16() as u32;
    }
    line.chars().count()
}

/// ファイルパスを `file://` URI 文字列へ。
fn uri_of(path: &Path) -> Option<String> {
    Url::from_file_path(path).ok().map(|u| u.to_string())
}

// ───────────────────────── 設定（言語 → 起動コマンド） ─────────────────────────

/// 1 言語ぶんのサーバー定義。
#[derive(Clone)]
struct LangServerDef {
    id: String,
    extensions: Vec<String>,
    command: Vec<String>,
}

/// 組み込みデフォルト + ユーザー設定をマージした言語サーバー表。
pub struct LspConfig {
    langs: Vec<LangServerDef>,
}

/// 組み込みデフォルト（ユーザーが最もよく使う Rust / PHP / Ruby）。
fn default_langs() -> Vec<LangServerDef> {
    let def = |id: &str, exts: &[&str], cmd: &[&str]| LangServerDef {
        id: id.to_string(),
        extensions: exts.iter().map(|s| s.to_string()).collect(),
        command: cmd.iter().map(|s| s.to_string()).collect(),
    };
    vec![
        def("rust", &["rs"], &["rust-analyzer"]),
        def("php", &["php", "phtml"], &["intelephense", "--stdio"]),
        def("ruby", &["rb", "rake", "gemspec"], &["ruby-lsp"]),
    ]
}

#[derive(Deserialize, Default)]
struct FileConfig {
    #[serde(default)]
    lsp: HashMap<String, LangEntry>,
}

#[derive(Deserialize)]
struct LangEntry {
    #[serde(default)]
    extensions: Vec<String>,
    #[serde(default)]
    command: Vec<String>,
}

impl LspConfig {
    /// デフォルト表に `~/.config/srev/config.toml` と `<root>/.srev.toml` をマージして読み込む。
    /// 同 languageId はユーザー設定で上書き。`command` を空にするとその言語を無効化する。
    pub fn load(root: &Path) -> Self {
        let mut langs = default_langs();
        if let Some(home) = std::env::var_os("HOME") {
            merge_file(&mut langs, &Path::new(&home).join(".config/srev/config.toml"));
        }
        merge_file(&mut langs, &root.join(".srev.toml"));
        Self { langs }
    }

    fn lang_for_ext(&self, ext: &str) -> Option<&LangServerDef> {
        self.langs
            .iter()
            .find(|l| !l.command.is_empty() && l.extensions.iter().any(|e| e == ext))
    }

    fn command_for(&self, id: &str) -> Option<Vec<String>> {
        self.langs
            .iter()
            .find(|l| l.id == id && !l.command.is_empty())
            .map(|l| l.command.clone())
    }
}

/// TOML 設定ファイルがあればパースして表へマージする（無ければ何もしない）。
fn merge_file(langs: &mut Vec<LangServerDef>, path: &Path) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let Ok(cfg) = toml::from_str::<FileConfig>(&text) else {
        return;
    };
    for (id, entry) in cfg.lsp {
        match langs.iter_mut().find(|l| l.id == id) {
            Some(existing) => {
                existing.command = entry.command;
                if !entry.extensions.is_empty() {
                    existing.extensions = entry.extensions;
                }
            }
            None => langs.push(LangServerDef {
                id,
                extensions: entry.extensions,
                command: entry.command,
            }),
        }
    }
}

// ───────────────────────── 1 言語サーバー（子プロセス） ─────────────────────────

struct LspServer {
    child: Child,
    out_tx: Sender<Value>,
    in_rx: Receiver<Value>,
    init_id: i64,
    initialized: bool,
    queue: Vec<Value>,
    opened: HashSet<String>,
    dead: bool,
}

impl LspServer {
    /// サーバーを起動し initialize 要求を送る。バイナリが無い等で失敗したら `None`。
    fn spawn(command: &[String], root: &Path, init_id: i64) -> Option<Self> {
        let mut child = Command::new(&command[0])
            .args(&command[1..])
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;

        let mut stdin = child.stdin.take()?;
        let stdout = child.stdout.take()?;

        // 書き込みスレッド: Value を Content-Length フレームで stdin へ。
        let (out_tx, out_rx) = channel::<Value>();
        thread::spawn(move || {
            while let Ok(msg) = out_rx.recv() {
                if write_message(&mut stdin, &msg).is_err() {
                    break;
                }
            }
        });

        // 読み取りスレッド: stdout の framed JSON-RPC を Value にして送る。
        let (in_tx, in_rx) = channel::<Value>();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            while let Some(msg) = read_message(&mut reader) {
                if in_tx.send(msg).is_err() {
                    break;
                }
            }
        });

        let server = Self {
            child,
            out_tx,
            in_rx,
            init_id,
            initialized: false,
            queue: Vec::new(),
            opened: HashSet::new(),
            dead: false,
        };
        // initialize はハンドシェイクの最初なので即送る（キューしない）。
        let _ = server.out_tx.send(request(init_id, "initialize", initialize_params(root)));
        Some(server)
    }

    /// 通知/要求を送る（initialize 完了前はキューし、完了後に flush）。
    fn send(&mut self, msg: Value) {
        if self.initialized {
            let _ = self.out_tx.send(msg);
        } else {
            self.queue.push(msg);
        }
    }
}

impl Drop for LspServer {
    fn drop(&mut self) {
        // 孤児プロセスを残さない。out_tx を落として書き込みスレッドも終わらせる。
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ───────────────────────── マネージャ（App が 1 個保持） ─────────────────────────

pub struct LspManager {
    root: PathBuf,
    config: LspConfig,
    servers: HashMap<String, LspServer>,
    unavailable: HashSet<String>,
    next_id: i64,
    /// 送信済み要求の応答の種別（Location 群か DocumentSymbol 群か）。
    kinds: HashMap<i64, ReqKind>,
}

/// 送信した要求の応答パーサ種別。
enum ReqKind {
    Locations,
    Symbols,
}

/// LSP 応答（要求 ID で App 側の用途に対応付ける）。
pub enum Reply {
    /// definition / references の結果。
    Locations(Vec<Loc>),
    /// documentSymbol の結果（アウトライン用、ドキュメント順にフラット化済み）。
    Symbols(Vec<SymInfo>),
}

/// アウトライン 1 項目（`character` は UTF-16 列）。
#[derive(Clone, Debug)]
pub struct SymInfo {
    pub name: String,
    pub kind: String,
    pub line: usize,
    pub character: u32,
}

impl LspManager {
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            config: LspConfig::load(root),
            servers: HashMap::new(),
            unavailable: HashSet::new(),
            next_id: 1,
            kinds: HashMap::new(),
        }
    }

    fn alloc_id(&mut self) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// パスの拡張子から languageId を解決する。
    pub fn lang_id_for(&self, path: &Path) -> Option<String> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        self.config.lang_for_ext(&ext).map(|l| l.id.clone())
    }

    /// 当該ファイルの言語サーバーが初期化済みか。
    pub fn is_ready_for(&self, path: &Path) -> bool {
        match self.lang_id_for(path) {
            Some(lang) => self
                .servers
                .get(&lang)
                .is_some_and(|s| s.initialized && !s.dead),
            None => false,
        }
    }

    /// 当該ファイルのサーバーを（必要なら起動して）確保し、未送信なら didOpen を送る。
    /// 戻り値 `true` は「今回初めて起動に失敗した（呼び出し側で一度知らせる）」。
    pub fn ensure_open(&mut self, path: &Path, lang_id: &str, text: &str) -> bool {
        if self.unavailable.contains(lang_id) {
            return false;
        }
        if !self.servers.contains_key(lang_id) {
            let Some(cmd) = self.config.command_for(lang_id) else {
                return false; // 設定が無い言語は黙ってフォールバック。
            };
            let id = self.alloc_id();
            match LspServer::spawn(&cmd, &self.root, id) {
                Some(s) => {
                    self.servers.insert(lang_id.to_string(), s);
                }
                None => {
                    self.unavailable.insert(lang_id.to_string());
                    return true;
                }
            }
        }
        if let Some(uri) = uri_of(path)
            && let Some(server) = self.servers.get_mut(lang_id)
            && server.opened.insert(uri.clone())
        {
            let params = json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": lang_id,
                    "version": 0,
                    "text": text,
                }
            });
            server.send(notification("textDocument/didOpen", params));
        }
        false
    }

    /// `textDocument/definition` を送り、要求 ID を返す（サーバー未準備なら `None`）。
    pub fn request_definition(&mut self, path: &Path, line: u32, character: u32) -> Option<i64> {
        let params = self.position_params(path, line, character, None)?;
        self.dispatch(path, "textDocument/definition", params, ReqKind::Locations)
    }

    /// `textDocument/references`（宣言を含めない）を送り、要求 ID を返す。
    pub fn request_references(&mut self, path: &Path, line: u32, character: u32) -> Option<i64> {
        let params =
            self.position_params(path, line, character, Some(json!({ "includeDeclaration": false })))?;
        self.dispatch(path, "textDocument/references", params, ReqKind::Locations)
    }

    /// `textDocument/documentSymbol` を送り、要求 ID を返す（アウトライン用）。
    pub fn request_document_symbol(&mut self, path: &Path) -> Option<i64> {
        let uri = uri_of(path)?;
        let params = json!({ "textDocument": { "uri": uri } });
        self.dispatch(path, "textDocument/documentSymbol", params, ReqKind::Symbols)
    }

    fn position_params(
        &self,
        path: &Path,
        line: u32,
        character: u32,
        context: Option<Value>,
    ) -> Option<Value> {
        let uri = uri_of(path)?;
        let mut params = json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character },
        });
        if let Some(ctx) = context {
            params["context"] = ctx;
        }
        Some(params)
    }

    /// 準備済みサーバーへ要求を送り、応答パーサ種別を控えて ID を返す。
    fn dispatch(&mut self, path: &Path, method: &str, params: Value, kind: ReqKind) -> Option<i64> {
        let lang = self.lang_id_for(path)?;
        let id = self.alloc_id();
        {
            let server = self.servers.get_mut(&lang)?;
            if !server.initialized || server.dead {
                return None;
            }
            server.send(request(id, method, params));
        }
        self.kinds.insert(id, kind);
        Some(id)
    }

    /// 毎フレーム呼ぶ。到着した応答を `(要求ID, 応答)` で返す。
    /// `Locations` が空なら「該当なし／エラー」で、呼び出し側で tags フォールバックする。
    pub fn poll(&mut self) -> Vec<(i64, Reply)> {
        let mut raw: Vec<(i64, Value)> = Vec::new();
        for server in self.servers.values_mut() {
            loop {
                match server.in_rx.try_recv() {
                    Ok(msg) => handle_message(server, msg, &mut raw),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        server.dead = true;
                        break;
                    }
                }
            }
        }
        raw.into_iter()
            .filter_map(|(id, result)| {
                let reply = match self.kinds.remove(&id)? {
                    ReqKind::Locations => Reply::Locations(parse_locations(&result)),
                    ReqKind::Symbols => Reply::Symbols(parse_symbols(&result)),
                };
                Some((id, reply))
            })
            .collect()
    }
}

/// サーバーからの 1 メッセージを処理する。応答は生の `result` を `out` へ積み、
/// パースは [`LspManager::poll`] が要求種別に応じて行う。
fn handle_message(server: &mut LspServer, msg: Value, out: &mut Vec<(i64, Value)>) {
    let has_method = msg.get("method").is_some();
    let id = msg.get("id");

    if let Some(id) = id
        && has_method
    {
        // サーバー → クライアント要求（configuration / registerCapability 等）。
        // 応答しないとサーバーが待ち続けることがあるため、null を返しておく。
        let _ = server.out_tx.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": Value::Null,
        }));
    } else if let Some(id) = id.and_then(Value::as_i64) {
        // 我々の要求への応答。
        if id == server.init_id {
            server.initialized = true;
            let _ = server.out_tx.send(notification("initialized", json!({})));
            for m in server.queue.drain(..) {
                let _ = server.out_tx.send(m);
            }
        } else {
            out.push((id, msg.get("result").cloned().unwrap_or(Value::Null)));
        }
    }
    // それ以外（サーバー通知: progress / diagnostics 等）は無視。
}

/// definition/references の結果（Location / Location[] / LocationLink[]）を正規化する。
fn parse_locations(result: &Value) -> Vec<Loc> {
    if result.is_null() {
        return Vec::new();
    }
    let Ok(resp) = serde_json::from_value::<GotoDefinitionResponse>(result.clone()) else {
        return Vec::new();
    };
    let to_loc = |uri: &Url, line: u32, character: u32| {
        uri.to_file_path().ok().map(|path| Loc {
            path,
            line: line as usize,
            character,
        })
    };
    match resp {
        GotoDefinitionResponse::Scalar(l) => to_loc(&l.uri, l.range.start.line, l.range.start.character)
            .into_iter()
            .collect(),
        GotoDefinitionResponse::Array(ls) => ls
            .iter()
            .filter_map(|l| to_loc(&l.uri, l.range.start.line, l.range.start.character))
            .collect(),
        GotoDefinitionResponse::Link(links) => links
            .iter()
            .filter_map(|l| {
                let r = l.target_selection_range.start;
                to_loc(&l.target_uri, r.line, r.character)
            })
            .collect(),
    }
}

/// documentSymbol の結果（フラット／階層）をドキュメント順のフラットな一覧へ。
/// 階層型は深さ優先で展開し、位置は名前の位置（selectionRange/location）を使う。
fn parse_symbols(result: &Value) -> Vec<SymInfo> {
    if result.is_null() {
        return Vec::new();
    }
    let Ok(resp) = serde_json::from_value::<DocumentSymbolResponse>(result.clone()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    match resp {
        DocumentSymbolResponse::Flat(infos) => {
            for si in infos {
                let p = si.location.range.start;
                out.push(SymInfo {
                    name: si.name,
                    kind: symbol_kind_str(si.kind).to_string(),
                    line: p.line as usize,
                    character: p.character,
                });
            }
        }
        DocumentSymbolResponse::Nested(syms) => {
            for s in &syms {
                flatten_symbol(s, &mut out);
            }
        }
    }
    out
}

fn flatten_symbol(s: &DocumentSymbol, out: &mut Vec<SymInfo>) {
    let p = s.selection_range.start;
    out.push(SymInfo {
        name: s.name.clone(),
        kind: symbol_kind_str(s.kind).to_string(),
        line: p.line as usize,
        character: p.character,
    });
    if let Some(children) = &s.children {
        for c in children {
            flatten_symbol(c, out);
        }
    }
}

/// LSP の `SymbolKind` を、アウトラインのグリフ解決（ui.rs `kind_glyph`）が
/// 認識する文字列へ。`SymbolKind` は newtype のため等価比較で振り分ける。
fn symbol_kind_str(k: SymbolKind) -> &'static str {
    use lsp_types::SymbolKind as K;
    if k == K::FUNCTION {
        "function"
    } else if k == K::METHOD {
        "method"
    } else if k == K::CONSTRUCTOR {
        "constructor"
    } else if k == K::CLASS {
        "class"
    } else if k == K::STRUCT {
        "struct"
    } else if k == K::INTERFACE {
        "interface"
    } else if k == K::ENUM {
        "enum"
    } else if k == K::MODULE || k == K::NAMESPACE || k == K::PACKAGE {
        "module"
    } else if k == K::CONSTANT || k == K::ENUM_MEMBER {
        "constant"
    } else {
        "symbol"
    }
}

// ───────────────────────── JSON-RPC メッセージ / フレーミング ─────────────────────────

fn request(id: i64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

fn notification(method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "method": method, "params": params })
}

fn initialize_params(root: &Path) -> Value {
    let root_uri = Url::from_directory_path(root).ok().map(|u| u.to_string());
    json!({
        "processId": std::process::id(),
        "rootUri": root_uri,
        "capabilities": {
            "textDocument": {
                "definition": { "dynamicRegistration": false, "linkSupport": true },
                "references": { "dynamicRegistration": false },
                "documentSymbol": {
                    "dynamicRegistration": false,
                    "hierarchicalDocumentSymbolSupport": true,
                },
            }
        },
    })
}

/// `Content-Length: N\r\n\r\n{json}` を 1 フレーム書く。
fn write_message(stdin: &mut impl Write, msg: &Value) -> std::io::Result<()> {
    let body = serde_json::to_vec(msg)?;
    write!(stdin, "Content-Length: {}\r\n\r\n", body.len())?;
    stdin.write_all(&body)?;
    stdin.flush()
}

/// 1 フレーム読む。EOF/不正なら `None`。
fn read_message(reader: &mut impl BufRead) -> Option<Value> {
    let mut content_len: usize = 0;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None; // EOF
        }
        let line = line.trim_end();
        if line.is_empty() {
            break; // ヘッダ終端
        }
        if let Some(v) = line.strip_prefix("Content-Length:") {
            content_len = v.trim().parse().ok()?;
        }
    }
    let mut buf = vec![0u8; content_len];
    std::io::Read::read_exact(reader, &mut buf).ok()?;
    serde_json::from_slice(&buf).ok()
}

/// LSP のリクエスト/応答のタイムアウト（これを過ぎたら tags フォールバック）。
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn utf16_roundtrip_ascii() {
        let line = "fn main() {}";
        assert_eq!(char_to_utf16(line, 3), 3);
        assert_eq!(utf16_to_char(line, 3), 3);
    }

    #[test]
    fn utf16_handles_multibyte_and_emoji() {
        // "あ" は UTF-16 で 1 ユニット、"👍" は 2 ユニット（サロゲートペア）。
        let line = "let x = \"あ👍\";";
        // char index 9 = 絵文字の直前（"...あ" の次）。
        let c = line.chars().position(|c| c == '👍').unwrap();
        let u = char_to_utf16(line, c);
        assert_eq!(utf16_to_char(line, u), c, "char↔utf16 must round-trip");
        // 絵文字を跨いだ列も復元できる。
        let after = char_to_utf16(line, c + 1);
        assert_eq!(after, u + 2, "emoji counts as 2 utf-16 units");
    }

    #[test]
    fn framing_roundtrip() {
        let msg = json!({ "jsonrpc": "2.0", "id": 7, "method": "x", "params": { "a": 1 } });
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).unwrap();
        let text = String::from_utf8(buf.clone()).unwrap();
        assert!(text.starts_with("Content-Length: "));
        let mut reader = BufReader::new(Cursor::new(buf));
        let got = read_message(&mut reader).expect("parse frame");
        assert_eq!(got, msg);
    }

    #[test]
    fn parse_locations_handles_variants() {
        // 単一 Location。
        let single = json!({
            "uri": "file:///tmp/a.rs",
            "range": { "start": {"line": 2, "character": 4}, "end": {"line": 2, "character": 8} }
        });
        let locs = parse_locations(&single);
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0].line, 2);
        assert_eq!(locs[0].character, 4);
        // 配列。
        let arr = json!([single]);
        assert_eq!(parse_locations(&arr).len(), 1);
        // null → 空。
        assert!(parse_locations(&Value::Null).is_empty());
    }

    #[test]
    fn parse_symbols_flat_and_nested() {
        // 階層型（DocumentSymbol）。子は親の直後にフラット化される。
        let nested = json!([
            {
                "name": "target", "kind": 12,
                "range": { "start": {"line":1,"character":0}, "end": {"line":1,"character":30} },
                "selectionRange": { "start": {"line":1,"character":9}, "end": {"line":1,"character":15} },
                "children": [
                    {
                        "name": "x", "kind": 13,
                        "range": { "start": {"line":2,"character":0}, "end": {"line":2,"character":5} },
                        "selectionRange": { "start": {"line":2,"character":2}, "end": {"line":2,"character":3} }
                    }
                ]
            }
        ]);
        let syms = parse_symbols(&nested);
        assert_eq!(syms.len(), 2);
        assert_eq!(syms[0].name, "target");
        assert_eq!(syms[0].kind, "function"); // SymbolKind 12
        assert_eq!((syms[0].line, syms[0].character), (1, 9)); // selectionRange の位置
        assert_eq!(syms[1].name, "x");
        assert_eq!(syms[1].line, 2);

        // フラット型（SymbolInformation）。
        let flat = json!([
            {
                "name": "Foo", "kind": 5,
                "location": {
                    "uri": "file:///t.php",
                    "range": { "start": {"line":3,"character":4}, "end": {"line":3,"character":7} }
                }
            }
        ]);
        let syms = parse_symbols(&flat);
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "Foo");
        assert_eq!(syms[0].kind, "class"); // SymbolKind 5
        assert_eq!((syms[0].line, syms[0].character), (3, 4));

        assert!(parse_symbols(&Value::Null).is_empty());
    }

    #[test]
    fn config_defaults_cover_rust_php_ruby() {
        let cfg = LspConfig {
            langs: default_langs(),
        };
        assert_eq!(cfg.lang_for_ext("rs").map(|l| l.id.as_str()), Some("rust"));
        assert_eq!(cfg.lang_for_ext("php").map(|l| l.id.as_str()), Some("php"));
        assert_eq!(cfg.lang_for_ext("phtml").map(|l| l.id.as_str()), Some("php"));
        assert_eq!(cfg.lang_for_ext("rb").map(|l| l.id.as_str()), Some("ruby"));
        assert!(cfg.lang_for_ext("xyz").is_none());
        assert_eq!(cfg.command_for("rust"), Some(vec!["rust-analyzer".to_string()]));
    }

    /// rust-analyzer を実起動して定義解決まで通すエンドツーエンド検証。
    /// 遅い＆環境依存（rust-analyzer 必須）のため通常は無視。手動実行:
    /// `cargo test --release -- --ignored rust_analyzer_resolves_definition`
    #[test]
    #[ignore = "spawns rust-analyzer; run with --ignored"]
    fn rust_analyzer_resolves_definition() {
        use std::time::Instant;

        // 小さな cargo プロジェクトを temp に用意（rust-analyzer は Cargo.toml を要求）。
        let dir = std::env::temp_dir().join(format!("srev_lsp_e2e_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"e2e\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        let line0 = "fn target() -> i32 { 1 }";
        let line1 = "fn main() { let _ = target(); }";
        let src = format!("{line0}\n{line1}\n");
        let main_rs = dir.join("src").join("main.rs");
        std::fs::write(&main_rs, &src).unwrap();

        let mut mgr = LspManager::new(&dir);
        if mgr.ensure_open(&main_rs, "rust", &src) {
            eprintln!("rust-analyzer unavailable; skipping");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }

        // `target()` 呼び出し位置（ASCII なので byte==utf16）。
        let ch = line1.find("target()").unwrap() as u32;

        // 初期化＆ワークスペース解析の完了を待ちつつ定期的に問い合わせ、
        // 非空の応答が来たら成功。
        let deadline = Instant::now() + Duration::from_secs(90);
        let mut pending = HashSet::new();
        let mut last_req = Instant::now() - Duration::from_secs(10);
        loop {
            // 初期化前は None が返る。準備でき次第ワークスペース解析の完了を待ちながら再送する。
            if last_req.elapsed() > Duration::from_secs(2) {
                if let Some(id) = mgr.request_definition(&main_rs, 1, ch) {
                    pending.insert(id);
                }
                last_req = Instant::now();
            }
            for (rid, reply) in mgr.poll() {
                if let Reply::Locations(locs) = reply
                    && pending.contains(&rid)
                    && !locs.is_empty()
                {
                    assert_eq!(locs[0].line, 0, "target() is defined on line 0");
                    assert!(locs[0].path.ends_with("main.rs"), "got {:?}", locs[0].path);
                    let _ = std::fs::remove_dir_all(&dir);
                    return;
                }
            }
            if Instant::now() > deadline {
                let _ = std::fs::remove_dir_all(&dir);
                panic!("rust-analyzer did not resolve definition within timeout");
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// intelephense を実起動して PHP の定義解決まで通すエンドツーエンド検証（手動）。
    #[test]
    #[ignore = "spawns intelephense; run with --ignored"]
    fn intelephense_resolves_definition() {
        use std::time::Instant;

        let dir = std::env::temp_dir().join(format!("srev_php_e2e_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let line0 = "<?php";
        let line1 = "function target() { return 1; }";
        let line2 = "$x = target();";
        let src = format!("{line0}\n{line1}\n{line2}\n");
        let main_php = dir.join("main.php");
        std::fs::write(&main_php, &src).unwrap();

        let mut mgr = LspManager::new(&dir);
        if mgr.ensure_open(&main_php, "php", &src) {
            eprintln!("[php-e2e] intelephense unavailable; skipping");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }

        let ch = line2.find("target()").unwrap() as u32;
        let deadline = Instant::now() + Duration::from_secs(90);
        let mut pending = HashSet::new();
        let mut last_req = Instant::now() - Duration::from_secs(10);
        loop {
            if last_req.elapsed() > Duration::from_secs(2) {
                if let Some(id) = mgr.request_definition(&main_php, 2, ch) {
                    pending.insert(id);
                }
                last_req = Instant::now();
            }
            for (rid, reply) in mgr.poll() {
                if let Reply::Locations(locs) = reply
                    && pending.contains(&rid)
                    && !locs.is_empty()
                {
                    assert_eq!(locs[0].line, 1, "target() defined on line 1");
                    let _ = std::fs::remove_dir_all(&dir);
                    return;
                }
            }
            if Instant::now() > deadline {
                let _ = std::fs::remove_dir_all(&dir);
                panic!("intelephense did not resolve definition within timeout");
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// intelephense から documentSymbol（アウトライン）を取得できることを実機検証（手動）。
    #[test]
    #[ignore = "spawns intelephense; run with --ignored"]
    fn intelephense_lists_symbols() {
        use std::time::Instant;

        let dir = std::env::temp_dir().join(format!("srev_php_sym_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = "<?php\nfunction target() { return 1; }\nclass Foo { public function bar() {} }\n";
        let main_php = dir.join("main.php");
        std::fs::write(&main_php, src).unwrap();

        let mut mgr = LspManager::new(&dir);
        if mgr.ensure_open(&main_php, "php", src) {
            eprintln!("[php-sym] intelephense unavailable; skipping");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }

        let deadline = Instant::now() + Duration::from_secs(90);
        let mut pending = HashSet::new();
        let mut last_req = Instant::now() - Duration::from_secs(10);
        loop {
            if last_req.elapsed() > Duration::from_secs(2) {
                if let Some(id) = mgr.request_document_symbol(&main_php) {
                    pending.insert(id);
                }
                last_req = Instant::now();
            }
            for (rid, reply) in mgr.poll() {
                if let Reply::Symbols(syms) = reply
                    && pending.contains(&rid)
                    && !syms.is_empty()
                {
                    let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
                    assert!(names.contains(&"target"), "symbols: {names:?}");
                    assert!(names.iter().any(|n| n.contains("Foo")), "symbols: {names:?}");
                    let _ = std::fs::remove_dir_all(&dir);
                    return;
                }
            }
            if Instant::now() > deadline {
                let _ = std::fs::remove_dir_all(&dir);
                panic!("intelephense did not return document symbols within timeout");
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn user_config_overrides_command_and_disables() {
        let mut langs = default_langs();
        let toml = r#"
            [lsp.rust]
            command = ["my-ra", "--stdio"]
            [lsp.ruby]
            command = []
            [lsp.go]
            extensions = ["go"]
            command = ["gopls"]
        "#;
        let cfg: FileConfig = toml::from_str(toml).unwrap();
        for (id, entry) in cfg.lsp {
            match langs.iter_mut().find(|l| l.id == id) {
                Some(e) => {
                    e.command = entry.command;
                    if !entry.extensions.is_empty() {
                        e.extensions = entry.extensions;
                    }
                }
                None => langs.push(LangServerDef {
                    id,
                    extensions: entry.extensions,
                    command: entry.command,
                }),
            }
        }
        let cfg = LspConfig { langs };
        assert_eq!(
            cfg.command_for("rust"),
            Some(vec!["my-ra".to_string(), "--stdio".to_string()])
        );
        // command 空 → 無効化（ext からも引けない）。
        assert!(cfg.lang_for_ext("rb").is_none());
        // 新言語の追加（再コンパイル無しの想定）。
        assert_eq!(cfg.lang_for_ext("go").map(|l| l.id.as_str()), Some("go"));
    }
}
