// Minimal MCP (Model Context Protocol) client.
//
// Scope: stdio transport, tools only (initialize / tools/list / tools/call).
// Servers are declared in %APPDATA%\Muse\mcp.json using the same
// `mcpServers` shape other MCP hosts use:
//
//   { "mcpServers": { "name": { "command": "npx", "args": ["-y", "..."],
//                               "env": { "KEY": "value" }, "cwd": "C:\\dir" } } }
//
// No async runtime: one reader thread per server, blocking requests with a
// timeout. Callers are expected to run requests off the UI thread.
//
// Trust model: the Muse web page can never reach this module. Tools are only
// invoked from the app's own local "MCP ツール" window, by an explicit click.

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};

pub const PROTOCOL_VERSION: &str = "2025-06-18";
const INIT_TIMEOUT: Duration = Duration::from_secs(90); // `npx -y` may download first
const LIST_TIMEOUT: Duration = Duration::from_secs(30);
const CALL_TIMEOUT: Duration = Duration::from_secs(300);
const STDERR_TAIL_BYTES: usize = 2000;

const CONFIG_TEMPLATE: &str = "{\n  \"mcpServers\": {\n  }\n}\n";

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------- config ----------

#[derive(Deserialize, Clone, Debug, Default)]
pub struct ServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub disabled: bool,
}

#[derive(Deserialize, Default)]
struct ConfigFile {
    #[serde(default, rename = "mcpServers")]
    mcp_servers: BTreeMap<String, Value>,
}

/// Parses mcp.json. Each server is parsed on its own so that one entry this
/// client cannot use (e.g. a remote `url` server) does not hide the others.
pub fn parse_config(text: &str) -> Result<BTreeMap<String, Result<ServerConfig, String>>, String> {
    let text = text.trim_start_matches('\u{feff}');
    if text.trim().is_empty() {
        return Ok(BTreeMap::new());
    }
    let file: ConfigFile =
        serde_json::from_str(text).map_err(|e| format!("mcp.json を読めません: {e}"))?;
    Ok(file
        .mcp_servers
        .into_iter()
        .map(|(name, raw)| {
            let parsed = if raw.get("command").is_none() && raw.get("url").is_some() {
                Err("URL 接続（HTTP）のサーバーには未対応です。command で起動する stdio サーバーだけ使えます。".to_string())
            } else {
                serde_json::from_value::<ServerConfig>(raw).map_err(|e| format!("設定が不正です: {e}"))
            };
            (name, parsed)
        })
        .collect())
}

fn load_config(path: &Path) -> Result<BTreeMap<String, Result<ServerConfig, String>>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse_config(&text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(format!("mcp.json を開けません: {e}")),
    }
}

// ---------- process helpers ----------

/// `Command::new("npx")` only finds `npx.exe`; Node tools ship as `npx.cmd`.
/// Resolve bare names through PATH × {.com,.exe,.bat,.cmd} ourselves and hand
/// std the full path — std then applies its own safe .cmd/.bat argument
/// escaping instead of us concatenating a `cmd /c` line by hand.
#[cfg(target_os = "windows")]
fn resolve_program(command: &str, path_override: Option<&str>) -> PathBuf {
    let direct = PathBuf::from(command);
    if command.contains(['/', '\\']) || direct.extension().is_some() {
        return direct;
    }
    let path_var = match path_override {
        Some(p) => std::ffi::OsString::from(p),
        None => std::env::var_os("PATH").unwrap_or_default(),
    };
    for dir in std::env::split_paths(&path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        for ext in ["com", "exe", "bat", "cmd"] {
            let candidate = dir.join(format!("{command}.{ext}"));
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    direct
}

#[cfg(not(target_os = "windows"))]
fn resolve_program(command: &str, _path_override: Option<&str>) -> PathBuf {
    PathBuf::from(command)
}

/// Kill-on-close job object: the whole server process tree (cmd → node → …)
/// dies when the client is dropped or when this app exits for any reason.
#[cfg(target_os = "windows")]
mod job {
    use std::ffi::c_void;

    #[repr(C)]
    struct BasicLimit {
        per_process_user_time_limit: i64,
        per_job_user_time_limit: i64,
        limit_flags: u32,
        minimum_working_set_size: usize,
        maximum_working_set_size: usize,
        active_process_limit: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
    }
    #[repr(C)]
    struct ExtendedLimit {
        basic: BasicLimit,
        io_info: [u64; 6],
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory_used: usize,
        peak_job_memory_used: usize,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateJobObjectW(attrs: *const c_void, name: *const u16) -> *mut c_void;
        fn SetInformationJobObject(job: *mut c_void, class: i32, info: *const c_void, len: u32) -> i32;
        fn AssignProcessToJobObject(job: *mut c_void, process: *mut c_void) -> i32;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }
    const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: i32 = 9;
    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x2000;

    pub struct Job(*mut c_void);
    // The handle is only closed on drop; safe to move/share across threads.
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    impl Job {
        pub fn attach(child: &std::process::Child) -> Option<Job> {
            use std::os::windows::io::AsRawHandle;
            unsafe {
                let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if handle.is_null() {
                    return None;
                }
                let job = Job(handle);
                let mut info: ExtendedLimit = std::mem::zeroed();
                info.basic.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let ok = SetInformationJobObject(
                    handle,
                    JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                    &info as *const ExtendedLimit as *const c_void,
                    std::mem::size_of::<ExtendedLimit>() as u32,
                );
                if ok == 0 || AssignProcessToJobObject(handle, child.as_raw_handle() as *mut c_void) == 0 {
                    return None; // dropping `job` closes the handle
                }
                Some(job)
            }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

// ---------- client (one per server process) ----------

type Pending = Arc<Mutex<HashMap<u64, mpsc::Sender<Value>>>>;

pub struct Client {
    child: Mutex<Child>,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Pending,
    next_id: AtomicU64,
    alive: Arc<AtomicBool>,
    stderr_tail: Arc<Mutex<String>>,
    server_info: Value,
    #[cfg(target_os = "windows")]
    _job: Option<job::Job>,
}

fn write_message(stdin: &Mutex<ChildStdin>, msg: &Value) -> std::io::Result<()> {
    // stdio framing: one JSON object per line. serde_json never emits raw
    // newlines, so the message is already a single line.
    let mut line = serde_json::to_string(msg).map_err(std::io::Error::other)?;
    line.push('\n');
    let mut stdin = lock(stdin);
    stdin.write_all(line.as_bytes())?;
    stdin.flush()
}

fn reader_loop(stdout: impl Read, stdin: Arc<Mutex<ChildStdin>>, pending: Pending, alive: Arc<AtomicBool>) {
    let mut reader = BufReader::new(stdout);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        // Servers occasionally print non-JSON noise on stdout; skip it.
        let Ok(msg) = serde_json::from_slice::<Value>(&buf) else { continue };
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        match msg.get("method").and_then(Value::as_str) {
            // Request from the server: answer the few we can, refuse the rest
            // so the server is never left waiting.
            Some(method) if !id.is_null() => {
                let reply = match method {
                    "ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
                    _ => json!({"jsonrpc": "2.0", "id": id,
                                "error": {"code": -32601, "message": "Method not found"}}),
                };
                let _ = write_message(&stdin, &reply);
            }
            Some(_) => {} // notification: nothing to do
            None => {
                if let Some(tx) = id.as_u64().and_then(|n| lock(&pending).remove(&n)) {
                    let _ = tx.send(msg);
                }
            }
        }
    }
    alive.store(false, Ordering::SeqCst);
    lock(&pending).clear(); // wakes every waiting request with "disconnected"
}

fn stderr_loop(stderr: impl Read, tail: Arc<Mutex<String>>) {
    let mut reader = BufReader::new(stderr);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let mut tail = lock(&tail);
        tail.push_str(&String::from_utf8_lossy(&buf));
        if tail.len() > STDERR_TAIL_BYTES * 2 {
            let mut cut = tail.len() - STDERR_TAIL_BYTES;
            while !tail.is_char_boundary(cut) {
                cut += 1;
            }
            tail.drain(..cut);
        }
    }
}

impl Client {
    /// Spawns the server and completes the MCP initialize handshake.
    pub fn connect(cfg: &ServerConfig) -> Result<Client, String> {
        if cfg.command.trim().is_empty() {
            return Err("command が空です".into());
        }
        let program = resolve_program(&cfg.command, cfg.env.get("PATH").map(String::as_str));
        let mut cmd = Command::new(&program);
        cmd.args(&cfg.args)
            .envs(&cfg.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = cfg.cwd.as_deref().filter(|c| !c.is_empty()) {
            cmd.current_dir(cwd);
        }
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW: no console flash
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("「{}」を起動できません: {e}", cfg.command))?;
        #[cfg(target_os = "windows")]
        let job = job::Job::attach(&child);

        let stdin = Arc::new(Mutex::new(child.stdin.take().expect("piped stdin")));
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let alive = Arc::new(AtomicBool::new(true));
        let stderr_tail = Arc::new(Mutex::new(String::new()));
        {
            let (stdin, pending, alive) = (stdin.clone(), pending.clone(), alive.clone());
            std::thread::spawn(move || reader_loop(stdout, stdin, pending, alive));
            let tail = stderr_tail.clone();
            std::thread::spawn(move || stderr_loop(stderr, tail));
        }

        let mut client = Client {
            child: Mutex::new(child),
            stdin,
            pending,
            next_id: AtomicU64::new(1),
            alive,
            stderr_tail,
            server_info: Value::Null,
            #[cfg(target_os = "windows")]
            _job: job,
        };
        let init = client.request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "muse-windows-anko", "version": env!("CARGO_PKG_VERSION")},
            }),
            INIT_TIMEOUT,
        )?;
        client.server_info = init.get("serverInfo").cloned().unwrap_or(Value::Null);
        write_message(&client.stdin, &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .map_err(|e| format!("サーバーへ送信できません: {e}"))?;
        Ok(client)
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    pub fn server_info(&self) -> &Value {
        &self.server_info
    }

    fn exit_reason(&self) -> String {
        // Give the stderr thread a moment to collect the last lines.
        std::thread::sleep(Duration::from_millis(100));
        let tail = lock(&self.stderr_tail);
        let tail = tail.trim();
        if tail.is_empty() {
            "サーバーが終了しました".to_string()
        } else {
            format!("サーバーが終了しました:\n{tail}")
        }
    }

    pub fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel();
        lock(&self.pending).insert(id, tx);
        if !self.is_alive() {
            lock(&self.pending).remove(&id);
            return Err(self.exit_reason());
        }
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if write_message(&self.stdin, &msg).is_err() {
            lock(&self.pending).remove(&id);
            return Err(self.exit_reason());
        }
        match rx.recv_timeout(timeout) {
            Ok(reply) => match reply.get("error") {
                Some(err) => {
                    let message = err.get("message").and_then(Value::as_str).unwrap_or("unknown error");
                    let code = err.get("code").and_then(Value::as_i64).unwrap_or(0);
                    Err(format!("{message} (code {code})"))
                }
                None => Ok(reply.get("result").cloned().unwrap_or(Value::Null)),
            },
            Err(mpsc::RecvTimeoutError::Timeout) => {
                lock(&self.pending).remove(&id);
                Err(format!("{} 秒待っても応答がありません ({method})", timeout.as_secs()))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(self.exit_reason()),
        }
    }

    pub fn list_tools(&self) -> Result<Vec<Value>, String> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..100 {
            let params = match &cursor {
                Some(c) => json!({"cursor": c}),
                None => json!({}),
            };
            let page = self.request("tools/list", params, LIST_TIMEOUT)?;
            if let Some(items) = page.get("tools").and_then(Value::as_array) {
                tools.extend(items.iter().cloned());
            }
            cursor = page.get("nextCursor").and_then(Value::as_str).map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        Ok(tools)
    }

    pub fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, String> {
        self.request("tools/call", json!({"name": name, "arguments": arguments}), CALL_TIMEOUT)
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let mut child = lock(&self.child);
        let _ = child.kill();
        let _ = child.wait();
        // `_job` drops after this and takes any grandchildren with it.
    }
}

/// Flattens a tools/call result into plain text for display / pasting.
pub fn render_tool_result(result: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    for item in result.get("content").and_then(Value::as_array).into_iter().flatten() {
        let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
        let text = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).map(str::to_owned);
        let part = match kind {
            "text" => text(item, "text"),
            "resource" => item.get("resource").and_then(|r| {
                text(r, "text").or_else(|| text(r, "uri").map(|u| format!("[resource: {u}]")))
            }),
            "resource_link" => text(item, "uri").map(|u| format!("[resource: {u}]")),
            "image" | "audio" => Some(format!(
                "[{kind}: {}]",
                item.get("mimeType").and_then(Value::as_str).unwrap_or("unknown type")
            )),
            _ => None,
        };
        parts.push(part.unwrap_or_else(|| item.to_string()));
    }
    if parts.is_empty() {
        if let Some(structured) = result.get("structuredContent") {
            parts.push(serde_json::to_string_pretty(structured).unwrap_or_default());
        }
    }
    parts.join("\n")
}

// ---------- manager (all configured servers) ----------

#[derive(Default)]
struct Slot {
    client: Option<Arc<Client>>,
    tools: Vec<Value>,
    error: Option<String>,
}

pub struct Manager {
    config_path: PathBuf,
    slots: Mutex<HashMap<String, Arc<Mutex<Slot>>>>,
}

impl Manager {
    pub fn new(config_path: PathBuf) -> Self {
        Self { config_path, slots: Mutex::new(HashMap::new()) }
    }

    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    /// Creates an empty mcp.json if there is none yet (never overwrites).
    pub fn ensure_config_file(&self) -> Result<(), String> {
        if self.config_path.exists() {
            return Ok(());
        }
        if let Some(dir) = self.config_path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::write(&self.config_path, CONFIG_TEMPLATE).map_err(|e| e.to_string())
    }

    fn slot(&self, name: &str) -> Arc<Mutex<Slot>> {
        lock(&self.slots).entry(name.to_owned()).or_default().clone()
    }

    /// Stops every server; the next `state`/`connect` re-reads mcp.json.
    pub fn reload(&self) {
        lock(&self.slots).clear();
    }

    fn server_json(name: &str, cfg: &Result<ServerConfig, String>, slot: Option<&Slot>) -> Value {
        let (command, disabled, config_error) = match cfg {
            Ok(c) => {
                let mut line = c.command.clone();
                for a in &c.args {
                    line.push(' ');
                    line.push_str(a);
                }
                (line, c.disabled, None)
            }
            Err(e) => (String::new(), false, Some(e.clone())),
        };
        let connected = slot.and_then(|s| s.client.as_ref()).is_some_and(|c| c.is_alive());
        let status = match (&config_error, disabled, slot) {
            (Some(_), _, _) => "error",
            (_, true, _) => "disabled",
            (_, _, None) => "connecting", // slot is locked by a connect in progress
            (_, _, Some(_)) if connected => "connected",
            (_, _, Some(s)) if s.error.is_some() => "error",
            _ => "idle",
        };
        json!({
            "name": name,
            "command": command,
            "status": status,
            "error": config_error.or_else(|| slot.and_then(|s| s.error.clone())),
            "tools": if connected { slot.map(|s| s.tools.clone()).unwrap_or_default() } else { Vec::new() },
            "serverInfo": slot.and_then(|s| s.client.as_ref()).map(|c| c.server_info().clone()),
        })
    }

    /// Snapshot for the UI. Never blocks on a server that is still starting.
    pub fn state(&self) -> Value {
        let (servers, config_error) = match load_config(&self.config_path) {
            Ok(cfg) => {
                let servers: Vec<Value> = cfg
                    .iter()
                    .map(|(name, c)| {
                        let slot = self.slot(name);
                        let guard = slot.try_lock().ok();
                        Self::server_json(name, c, guard.as_deref())
                    })
                    .collect();
                (servers, None)
            }
            Err(e) => (Vec::new(), Some(e)),
        };
        json!({
            "configPath": self.config_path.display().to_string(),
            "configError": config_error,
            "servers": servers,
        })
    }

    fn config_for(&self, name: &str) -> Result<ServerConfig, String> {
        let mut all = load_config(&self.config_path)?;
        let cfg = all
            .remove(name)
            .ok_or_else(|| format!("mcp.json に「{name}」がありません"))??;
        if cfg.disabled {
            return Err(format!("「{name}」は disabled になっています"));
        }
        Ok(cfg)
    }

    fn connect_locked(&self, name: &str, slot: &mut Slot) -> Result<Arc<Client>, String> {
        if let Some(client) = slot.client.as_ref().filter(|c| c.is_alive()) {
            return Ok(client.clone());
        }
        slot.client = None;
        slot.tools.clear();
        let attempt = self.config_for(name).and_then(|cfg| {
            let client = Client::connect(&cfg)?;
            let tools = client.list_tools()?;
            Ok((Arc::new(client), tools))
        });
        match attempt {
            Ok((client, tools)) => {
                slot.client = Some(client.clone());
                slot.tools = tools;
                slot.error = None;
                Ok(client)
            }
            Err(e) => {
                slot.error = Some(e.clone());
                Err(e)
            }
        }
    }

    /// Starts the server if needed and returns its UI snapshot (with tools).
    pub fn connect(&self, name: &str) -> Value {
        let slot = self.slot(name);
        let mut guard = lock(&slot);
        let _ = self.connect_locked(name, &mut guard);
        let cfg = load_config(&self.config_path)
            .ok()
            .and_then(|mut all| all.remove(name))
            .unwrap_or_else(|| Err(format!("mcp.json に「{name}」がありません")));
        Self::server_json(name, &cfg, Some(&guard))
    }

    pub fn call(&self, server: &str, tool: &str, arguments: Value) -> Result<Value, String> {
        let client = {
            let slot = self.slot(server);
            let mut guard = lock(&slot);
            self.connect_locked(server, &mut guard)?
        };
        let result = client.call_tool(tool, arguments)?;
        Ok(json!({
            "text": render_tool_result(&result),
            "isError": result.get("isError").and_then(Value::as_bool).unwrap_or(false),
        }))
    }

    /// Dispatches one request from the MCP window. Blocking: call off the UI thread.
    pub fn handle(&self, msg: &Value) -> Result<Value, String> {
        let field = |key: &str| msg.get(key).and_then(Value::as_str).unwrap_or_default();
        match field("cmd") {
            "state" => Ok(self.state()),
            "reload" => {
                self.reload();
                Ok(self.state())
            }
            "connect" => Ok(self.connect(field("server"))),
            "call" => {
                let args = msg.get("args").cloned().unwrap_or_else(|| json!({}));
                if !args.is_object() {
                    return Err("引数は JSON オブジェクト { … } で指定してください".into());
                }
                self.call(field("server"), field("tool"), args)
            }
            other => Err(format!("unknown command: {other}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_parses_per_server() {
        let cfg = parse_config(
            "\u{feff}{\"mcpServers\": {
                \"ok\": {\"command\": \"node\", \"args\": [\"a.js\"], \"env\": {\"K\": \"v\"}},
                \"remote\": {\"url\": \"https://example.com/mcp\"},
                \"broken\": {\"args\": []}
            }}",
        )
        .unwrap();
        let ok = cfg["ok"].as_ref().unwrap();
        assert_eq!(ok.command, "node");
        assert_eq!(ok.args, ["a.js"]);
        assert_eq!(ok.env["K"], "v");
        assert!(cfg["remote"].as_ref().unwrap_err().contains("URL"));
        assert!(cfg["broken"].is_err());
        assert!(parse_config("").unwrap().is_empty());
        assert!(parse_config("{}").unwrap().is_empty());
        assert!(parse_config("{ nope").is_err());
    }

    #[test]
    fn renders_tool_results() {
        let r = json!({"content": [
            {"type": "text", "text": "hello"},
            {"type": "image", "mimeType": "image/png", "data": "AAAA"},
            {"type": "resource", "resource": {"uri": "file:///a", "text": "body"}},
            {"type": "resource_link", "uri": "file:///b"}
        ]});
        assert_eq!(render_tool_result(&r), "hello\n[image: image/png]\nbody\n[resource: file:///b]");
        let s = json!({"content": [], "structuredContent": {"a": 1}});
        assert!(render_tool_result(&s).contains("\"a\": 1"));
    }

    fn node_available() -> bool {
        Command::new(resolve_program("node", None))
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    fn mock_config(extra: &[&str]) -> ServerConfig {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("mock_mcp_server.js");
        let mut args = vec![script.display().to_string()];
        args.extend(extra.iter().map(|s| s.to_string()));
        ServerConfig { command: "node".into(), args, ..Default::default() }
    }

    #[test]
    fn talks_to_a_real_stdio_server() {
        if !node_available() {
            eprintln!("skipped: node is not installed");
            return;
        }
        let client = Client::connect(&mock_config(&[])).expect("handshake");
        assert_eq!(client.server_info()["name"], "mock");

        // Two pages, with a server→client ping in between that must be answered.
        let tools = client.list_tools().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["echo", "fail"]);

        let out = client.call_tool("echo", json!({"text": "あんこ\n2行目"})).unwrap();
        assert_eq!(render_tool_result(&out), "あんこ\n2行目");

        let err = client.call_tool("fail", json!({})).unwrap();
        assert_eq!(err["isError"], true);

        let rpc_err = client.call_tool("nope", json!({})).unwrap_err();
        assert!(rpc_err.contains("Unknown tool"), "{rpc_err}");
    }

    #[test]
    fn reports_a_server_that_dies() {
        if !node_available() {
            eprintln!("skipped: node is not installed");
            return;
        }
        let err = Client::connect(&mock_config(&["--crash"])).err().expect("must fail");
        assert!(err.contains("boom on stderr"), "{err}");

        let missing = ServerConfig { command: "no-such-mcp-server-xyz".into(), ..Default::default() };
        assert!(Client::connect(&missing).is_err());
    }

    #[test]
    fn manager_round_trip() {
        if !node_available() {
            eprintln!("skipped: node is not installed");
            return;
        }
        let dir = std::env::temp_dir().join(format!("anko-mcp-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mcp.json");
        let mgr = Manager::new(path.clone());
        mgr.ensure_config_file().unwrap();
        assert_eq!(mgr.state()["servers"].as_array().unwrap().len(), 0);

        let cfg = mock_config(&[]);
        std::fs::write(
            &path,
            json!({"mcpServers": {"mock": {"command": cfg.command, "args": cfg.args}}}).to_string(),
        )
        .unwrap();
        assert_eq!(mgr.state()["servers"][0]["status"], "idle");

        let connected = mgr.handle(&json!({"cmd": "connect", "server": "mock"})).unwrap();
        assert_eq!(connected["status"], "connected");
        assert_eq!(connected["tools"].as_array().unwrap().len(), 2);

        let out = mgr
            .handle(&json!({"cmd": "call", "server": "mock", "tool": "echo", "args": {"text": "hi"}}))
            .unwrap();
        assert_eq!(out, json!({"text": "hi", "isError": false}));

        assert!(mgr.handle(&json!({"cmd": "call", "server": "mock", "tool": "echo", "args": []})).is_err());
        assert!(mgr.handle(&json!({"cmd": "call", "server": "ghost", "tool": "x"})).is_err());

        let reloaded = mgr.handle(&json!({"cmd": "reload"})).unwrap();
        assert_eq!(reloaded["servers"][0]["status"], "idle");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn resolves_cmd_shims() {
        let dir = std::env::temp_dir().join(format!("anko-mcp-path-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("mytool.cmd"), "@echo off\r\n").unwrap();
        let path = dir.display().to_string();
        assert_eq!(resolve_program("mytool", Some(&path)), dir.join("mytool.cmd"));
        assert_eq!(resolve_program("absent", Some(&path)), PathBuf::from("absent"));
        assert_eq!(resolve_program("x.exe", Some(&path)), PathBuf::from("x.exe"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
