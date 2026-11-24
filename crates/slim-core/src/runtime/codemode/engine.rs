//! One disposable interpreter per cell. Only JSON crosses the host boundary.
//!
//! `tools.call` is a real promise: the worker sends the call with a ticket
//! and keeps running the program; the host answers on the completion channel,
//! in whatever order the calls finish. The worker is its own event loop: it
//! runs the pending jobs, and while the cell's promise is pending it waits for
//! the next completion. Once the program returns, calls it did not await
//! still run to the end. Helpers (`searchTools`, `describeTool`,
//! `listServers`, `store`, `load`) stay synchronous and block until the host
//! replies. Time spent waiting for the host does not count against the
//! JavaScript CPU limit; the wall deadline covers everything.
use std::cell::{Cell as Counter, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc, Arc, Condvar, LazyLock, Mutex,
};
use std::thread;
use std::time::{Duration, Instant};

use rquickjs::{
    Array, CaughtError, Context, Ctx, Exception, Function, Module, Object, Persistent, Promise,
    WriteOptions, WriteOptionsEndianness,
};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::mpsc::{channel, Receiver};
use tokio::sync::oneshot;

use crate::runtime::CancellationToken;

pub(super) const MAX_JSON_BYTES: usize = 1024 * 1024;
pub(super) const MAX_CODE_BYTES: usize = 64 * 1024;
/// Console output kept per cell, the bound of a returned value; the rest is
/// counted, not stored. What exceeds the result budget is not lost: the cell
/// output becomes an artifact like any long tool output.
pub(super) const MAX_CONSOLE_BYTES: usize = MAX_JSON_BYTES;
const INTERRUPTED: &str = "cell cancelled or deadline exceeded";
const HOST_DISCONNECTED: &str = "CodeMode host disconnected";
const DEADLOCK: &str =
    "deadlock: the program awaits a promise that can never settle (no tool call is in flight)";
const MAX_UNHANDLED_REJECTIONS: usize = 16;
const DEDICATED_JS_WORKERS: usize = 2;
const REQUEST_CHANNEL_CAPACITY: usize = 64;

thread_local! {
    static WARM_RUNTIME: RefCell<Option<rquickjs::Runtime>> = const { RefCell::new(None) };
}

static PRELUDE_BYTECODE: LazyLock<Option<Vec<u8>>> = LazyLock::new(|| {
    let runtime = rquickjs::Runtime::new().ok()?;
    let context = Context::full(&runtime).ok()?;
    context.with(|ctx| {
        let src = format!("export default {PRELUDE};");
        let module = Module::declare(ctx, "slim_prelude", src).ok()?;
        module
            .write(WriteOptions {
                endianness: WriteOptionsEndianness::Native,
                strip_source: true,
                strip_debug: true,
                ..Default::default()
            })
            .ok()
    })
});

struct CellJob {
    code: String,
    run: CancellationToken,
    stop: CancellationToken,
    deadline: Instant,
    cpu: Duration,
    sender: tokio::sync::mpsc::Sender<Request>,
    natives: Vec<String>,
    wake_rx: mpsc::Receiver<WorkerWake>,
    reply: oneshot::Sender<Output>,
}

enum WorkerMsg {
    Run(Box<CellJob>),
    Prewarm,
}

struct WorkerShared {
    queue: Mutex<VecDeque<WorkerMsg>>,
    cvar: Condvar,
    busy: AtomicUsize,
    prewarm_pending: AtomicBool,
}

static WORKER_POOL: LazyLock<Arc<WorkerShared>> = LazyLock::new(|| {
    let shared = Arc::new(WorkerShared {
        queue: Mutex::new(VecDeque::new()),
        cvar: Condvar::new(),
        busy: AtomicUsize::new(0),
        prewarm_pending: AtomicBool::new(false),
    });
    for idx in 0..DEDICATED_JS_WORKERS {
        let pool = Arc::clone(&shared);
        let _ = thread::Builder::new()
            .name(format!("slim-codemode-js-{idx}"))
            .spawn(move || js_worker_loop(pool));
    }
    shared
});

fn configure_runtime(runtime: &rquickjs::Runtime) {
    runtime.set_memory_limit(32 * 1024 * 1024);
    runtime.set_max_stack_size(256 * 1024);
}

fn js_worker_loop(shared: Arc<WorkerShared>) {
    let mut warm_runtime: Option<rquickjs::Runtime> = None;
    let mut warm_context: Option<Context> = None;

    loop {
        let msg = {
            let mut q = shared
                .queue
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            loop {
                if let Some(m) = q.pop_front() {
                    if matches!(m, WorkerMsg::Prewarm) {
                        shared.prewarm_pending.store(false, Ordering::Release);
                    }
                    break m;
                }
                q = shared
                    .cvar
                    .wait(q)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
        };

        match msg {
            WorkerMsg::Prewarm => {
                if warm_runtime.is_none() {
                    if let Ok(rt) = rquickjs::Runtime::new() {
                        configure_runtime(&rt);
                        warm_runtime = Some(rt);
                    }
                }
                if warm_context.is_none() {
                    if let Some(rt) = warm_runtime.as_ref() {
                        warm_context = Context::full(rt).ok();
                    }
                }
                let _ = &*PRELUDE_BYTECODE;
            }
            WorkerMsg::Run(job) => {
                struct SlotRelease<'a>(&'a AtomicUsize);
                impl Drop for SlotRelease<'_> {
                    fn drop(&mut self) {
                        self.0.fetch_sub(1, Ordering::AcqRel);
                    }
                }
                let _release = SlotRelease(&shared.busy);
                let _work = job.run.track_native_work();
                let console = Rc::new(RefCell::new(Console::default()));
                let _ = interpret_with_slot(
                    &mut warm_runtime,
                    &mut warm_context,
                    &job.code,
                    &job.run,
                    &job.stop,
                    job.deadline,
                    job.cpu,
                    job.sender,
                    &job.natives,
                    &console,
                    job.wake_rx,
                    Some(job.reply),
                );
                // Pre-create next clean Context off the critical path while waiting for next cell.
                if warm_context.is_none() {
                    if let Some(rt) = warm_runtime.as_ref() {
                        warm_context = Context::full(rt).ok();
                    }
                }
            }
        }
    }
}

/// Hints the dedicated QuickJS worker pool to initialize a warm `Runtime` +
/// clean `Context` + `PRELUDE` bytecode ahead of tool dispatch (non-blocking).
pub(crate) fn prewarm_worker_cell() {
    let shared = &*WORKER_POOL;
    if shared.busy.load(Ordering::Acquire) >= DEDICATED_JS_WORKERS {
        return;
    }
    if !shared.prewarm_pending.swap(true, Ordering::AcqRel) {
        if let Ok(mut q) = shared.queue.try_lock() {
            if q.is_empty() {
                q.push_back(WorkerMsg::Prewarm);
                shared.cvar.notify_one();
            } else {
                shared.prewarm_pending.store(false, Ordering::Release);
            }
        } else {
            shared.prewarm_pending.store(false, Ordering::Release);
        }
    }
}

#[cfg(test)]
pub(crate) fn prelude_bytecode_len() -> Option<usize> {
    PRELUDE_BYTECODE.as_ref().map(Vec::len)
}

/// A synchronous helper request.
#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Command {
    Store { key: String, value: Value },
    Load { key: String },
    SearchTools(SearchArgs),
    DescribeTool { name: String },
    ListServers,
    Finish { answer: String },
    ReadArtifact { args: Value },
    BeginAtomic,
    CommitAtomic,
    RollbackAtomic,
}

/// Arguments of `searchTools(query, {limit, server})`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SearchArgs {
    pub query: String,
    pub limit: Option<usize>,
    pub server: Option<String>,
}

pub(super) enum Request {
    /// `tools.call`: the outcome comes back as the completion of `ticket`.
    Call {
        ticket: u32,
        name: String,
        arguments: Value,
        raw_arguments: String,
    },
    /// A synchronous helper; the worker blocks on `WorkerWake::HelperReply`.
    Helper { command: Command },
    /// The program's promise settled; calls still in flight were not
    /// awaited.
    Returned,
}

/// Wall and JavaScript CPU limits of a cell.
#[derive(Clone, Copy)]
pub(super) struct Limits {
    pub wall: Duration,
    pub cpu: Duration,
}

/// What a cell produced: its result, captured console and unhandled JS rejections.
pub(super) struct Output {
    pub result: Result<String, String>,
    console: Console,
    unhandled_rejections: Vec<String>,
}

impl Output {
    pub fn failed(error: String) -> Self {
        Self {
            result: Err(error),
            console: Console::default(),
            unhandled_rejections: Vec::new(),
        }
    }

    pub fn has_unhandled_rejections(&self) -> bool {
        !self.unhandled_rejections.is_empty()
    }

    /// The result followed by `note`, unhandled rejections and the console, when there are any.
    pub fn finish(self, note: Option<&str>) -> Result<String, String> {
        let rejections = (!self.unhandled_rejections.is_empty()).then(|| {
            format!(
                "unhandled promise rejection(s): {}",
                self.unhandled_rejections.join("; ")
            )
        });
        let attach = |mut text: String| {
            if let Some(note) = note {
                text.push('\n');
                text.push_str(note);
            }
            if let Some(rejections) = &rejections {
                text.push('\n');
                text.push_str(rejections);
            }
            self.console.attach(text)
        };
        match self.result {
            Ok(output) => Ok(attach(output)),
            Err(error) => Err(attach(error)),
        }
    }
}

/// Typed outcome delivered across the JS bridge.
#[derive(Clone, Debug)]
pub(super) enum Payload {
    /// Raw string resolved directly into JS without `JSON.parse` (`slim.*` text outputs).
    Text(String),
    /// Serialized MCP envelope JSON (`isError` rejects, `structuredContent ?? result` resolves).
    McpJson(String),
    /// Serialized JSON value resolved via `JSON.parse` (`slim.glob`, `structured: true`).
    #[allow(dead_code)]
    Json(String),
    /// Pre-parsed JSON value converted directly into native JS values without stringify/parse.
    JsonValue(Arc<Value>),
}

impl From<Value> for Payload {
    fn from(value: Value) -> Self {
        Self::McpJson(value.to_string())
    }
}

/// Event delivered to the worker thread without polling.
enum WorkerWake {
    Completion {
        ticket: u32,
        result: Result<Payload, String>,
    },
    HelperReply(Result<Value, String>),
    Interrupt,
}

pub(super) struct Cell {
    pub requests: Receiver<Request>,
    pub worker: tokio::task::JoinHandle<Output>,
    /// Cancelled by the run, by the deadline, by the host or on drop. Native
    /// calls observe it directly and MCP calls through its MCP handle.
    pub cancellation: CancellationToken,
    pub deadline: Instant,
    wake: mpsc::Sender<WorkerWake>,
}

impl Cell {
    /// Settles the promise of `ticket`. A worker that already stopped no
    /// longer listens, which is not an error.
    pub fn settle(&self, ticket: u32, result: Result<impl Into<Payload>, String>) {
        let _ = self.wake.send(WorkerWake::Completion {
            ticket,
            result: result.map(Into::into),
        });
    }

    /// Rejects the promise of `ticket` with an error message.
    pub fn reject(&self, ticket: u32, error: impl Into<String>) {
        let _ = self.wake.send(WorkerWake::Completion {
            ticket,
            result: Err(error.into()),
        });
    }

    /// Delivers the reply to a synchronous `Request::Helper`.
    pub fn reply_helper(&self, result: Result<Value, String>) {
        let _ = self.wake.send(WorkerWake::HelperReply(result));
    }
}

impl Drop for Cell {
    fn drop(&mut self) {
        self.cancellation.cancel();
        let _ = self.wake.send(WorkerWake::Interrupt);
    }
}

/// Returns an initializer `(callHost, helper, log, natives, nativeGrep, nativeDiff) => settle`.
/// `natives` names the `slim.<name>` functions (Code mode only).
const PRELUDE: &str = r#"
((callHost, helper, log, natives, nativeGrep, nativeDiff) => {
    const { stringify, parse } = JSON;
    const runHelper = value => helper(stringify(value));
    const pending = new Map();
    const call = (name, arguments_) => new Promise((resolve, reject) => {
        const json = stringify(arguments_ ?? {});
        if (json === undefined) throw new TypeError('tool arguments must be JSON-serializable');
        pending.set(callHost(name, json), {resolve, reject});
    });
    const attachLazyLines = obj => {
        if (!obj || typeof obj !== 'object' || typeof obj.content !== 'string' || Object.prototype.hasOwnProperty.call(obj, 'lines')) {
            return obj;
        }
        let cachedLines = null;
        Object.defineProperty(obj, 'lines', {
            enumerable: true,
            configurable: true,
            get() {
                if (cachedLines === null) {
                    const s = obj.content;
                    if (obj.total_lines === 0 || s.length === 0) {
                        cachedLines = [];
                    } else {
                        const trimmed = s.endsWith('\r\n') ? s.slice(0, -2) : (s.endsWith('\n') || s.endsWith('\r')) ? s.slice(0, -1) : s;
                        cachedLines = trimmed.split(/\r?\n/);
                    }
                }
                return cachedLines;
            },
        });
        return obj;
    };
    globalThis.tools = Object.freeze({call});
    if (natives.length > 0) {
        const slimObj = Object.fromEntries(
            natives.map(name => [name, async args => {
                const res = await call('slim.' + name, args ?? {});
                return name === 'read' && args?.structured ? attachLazyLines(res) : res;
            }]));
        const finishFn = answer => {
            if (typeof answer !== 'string' || answer.trim().length === 0) {
                throw new TypeError('finish() expects a non-empty string answer');
            }
            runHelper({op: 'finish', answer});
        };
        Object.defineProperty(slimObj, 'readMany', {value: async (paths, opts) => {
            if (!Array.isArray(paths)) throw new TypeError('slim.readMany() expects an array of paths');
            if (paths.length === 0) return {};
            if (natives.includes('read')) {
                const map = await call('slim.readMany', { paths, opts: opts ?? {} });
                if (opts?.structured && map && typeof map === 'object') {
                    for (const k of Object.keys(map)) attachLazyLines(map[k]);
                }
                return map;
            }
            const base = opts ?? {};
            const settled = await Promise.all(paths.map(async entry => {
                const arg = typeof entry === 'string'
                    ? Object.assign({}, base, { path: entry })
                    : Object.assign({}, base, entry);
                return [arg.path, await slimObj.read(arg)];
            }));
            return Object.fromEntries(settled);
        }});
        if (natives.includes('read')) {
            Object.defineProperty(slimObj, 'outlineMany', {value: async (pathsOrReq, opts) => {
                const payload = Array.isArray(pathsOrReq)
                    ? Object.assign({}, opts ?? {}, { paths: pathsOrReq })
                    : (pathsOrReq ?? {});
                return call('slim.outlineMany', payload);
            }});
            Object.defineProperty(slimObj, 'grepIndex', {value: async (queriesOrReq, opts) => {
                const payload = typeof queriesOrReq === 'string'
                    ? Object.assign({}, opts ?? {}, { queries: [queriesOrReq] })
                    : Array.isArray(queriesOrReq)
                        ? Object.assign({}, opts ?? {}, { queries: queriesOrReq })
                        : (queriesOrReq ?? {});
                return call('slim.grepIndex', payload);
            }});
            Object.defineProperty(slimObj, 'graph', {value: async req => {
                return call('slim.graph', req ?? {});
            }});
            Object.defineProperty(slimObj, 'readArtifact', {value: async args => {
                return runHelper({op: 'read_artifact', args: args ?? {}});
            }});
        }
        Object.defineProperty(slimObj, 'patchMany', {value: async specs => {
            if (!Array.isArray(specs)) throw new TypeError('slim.patchMany() expects an array of patch specs');
            return Promise.all(specs.map(spec => slimObj.patch(spec)));
        }});
        Object.defineProperty(slimObj, 'atomic', {value: async fn => {
            if (typeof fn !== 'function') throw new TypeError('slim.atomic() expects a function');
            runHelper({op: 'begin_atomic'});
            try {
                const value = await fn();
                runHelper({op: 'commit_atomic'});
                return value;
            } catch (err) {
                try { runHelper({op: 'rollback_atomic'}); } catch {}
                throw err;
            }
        }});
        globalThis.finish = finishFn;
        Object.defineProperty(slimObj, 'finish', {value: finishFn});
        globalThis.slim = Object.freeze(slimObj);
    }
    const lines = text => {
        const s = String(text ?? '');
        return s.length === 0 ? [] : s.split(/\r?\n/);
    };
    const grepText = (text, pattern, opts) => {
        const cfg = opts ?? {};
        const ctx = Math.max(0, (cfg.context ?? 0) | 0);
        if (pattern instanceof RegExp) {
            const raw = lines(text);
            const flags = pattern.flags.replace(/g/g, '') + (cfg.ignoreCase && !pattern.flags.includes('i') ? 'i' : '');
            const re = new RegExp(pattern.source, flags);
            const out = [];
            for (let i = 0; i < raw.length; i++) {
                if (re.test(raw[i])) {
                    const start = Math.max(0, i - ctx);
                    const end = Math.min(raw.length - 1, i + ctx);
                    const context = [];
                    for (let j = start; j <= end; j++) {
                        if (j !== i) context.push({ line: j + 1, text: raw[j] });
                    }
                    out.push({ line: i + 1, text: raw[i], context });
                }
            }
            return out;
        }
        return nativeGrep(String(text ?? ''), String(pattern), Boolean(cfg.ignoreCase), ctx);
    };
    const diffText = (before, after, opts) =>
        nativeDiff(String(before ?? ''), String(after ?? ''), Math.max(0, (opts?.context ?? 2) | 0));
    const parseJsonSafe = (s, fallback = null) => {
        if (typeof s !== 'string') return fallback;
        try { return parse(s); } catch { return fallback; }
    };
    const chunk = (arr, size) => {
        if (!Array.isArray(arr)) throw new TypeError('chunk() expects an array');
        const step = Math.max(1, (size | 0) || 1);
        const out = [];
        for (let i = 0; i < arr.length; i += step) out.push(arr.slice(i, i + step));
        return out;
    };
    const unique = (arr, keyFn) => {
        if (!Array.isArray(arr)) throw new TypeError('unique() expects an array');
        const seen = new Set();
        const out = [];
        for (const item of arr) {
            const key = keyFn ? keyFn(item) : item;
            if (!seen.has(key)) { seen.add(key); out.push(item); }
        }
        return out;
    };
    globalThis.lines = lines;
    globalThis.grepText = grepText;
    globalThis.diffText = diffText;
    globalThis.parseJsonSafe = parseJsonSafe;
    globalThis.chunk = chunk;
    globalThis.unique = unique;
    const text = value => {
        if (typeof value === 'string') return value;
        try {
            const json = stringify(value);
            return json === undefined ? String(value) : json;
        } catch {
            return String(value);
        }
    };
    const write = prefix => (...values) => log(prefix + values.map(text).join(' '));
    globalThis.console = Object.freeze({
        log: write(''), info: write(''), warn: write('warn: '), error: write('error: '),
    });
    globalThis.searchTools = (query, options) => {
        if (typeof query !== 'string') throw new TypeError('searchTools() expects a query string');
        const {limit, server} = options ?? {};
        if (limit !== undefined && (!Number.isInteger(limit) || limit <= 0)) {
            throw new TypeError('searchTools() limit must be a positive integer');
        }
        if (server !== undefined && typeof server !== 'string') {
            throw new TypeError('searchTools() server must be a string');
        }
        return runHelper({op: 'search_tools', query, limit, server});
    };
    globalThis.describeTool = name => {
        if (typeof name !== 'string') throw new TypeError('describeTool() expects a tool name');
        return runHelper({op: 'describe_tool', name});
    };
    globalThis.listServers = () => runHelper({op: 'list_servers'});
    globalThis.store = (key, value) => {
        if (value === undefined) throw new TypeError('store() requires a JSON value (use null to delete)');
        return runHelper({op: 'store', key, value});
    };
    globalThis.load = key => runHelper({op: 'load', key});
    return (ticket, kind, payload) => {
        const entry = pending.get(ticket);
        if (entry === undefined) return;
        pending.delete(ticket);
        if (kind === 0) return entry.reject(new Error(payload));
        if (kind === 1 || kind === 4) return entry.resolve(payload);
        if (kind === 3) return entry.resolve(parse(payload));
        const result = parse(payload);
        if (result?.isError) return entry.reject(new Error(payload));
        entry.resolve(result?.structuredContent ?? result);
    };
})
"#;

/// Console lines of one cell, bounded by [`MAX_CONSOLE_BYTES`].
#[derive(Default)]
struct Console {
    text: String,
    dropped: usize,
}

impl Console {
    fn push(&mut self, line: &str) {
        if self.dropped > 0 {
            self.dropped += line.len();
            return;
        }
        let separator = usize::from(!self.text.is_empty());
        let room = MAX_CONSOLE_BYTES.saturating_sub(self.text.len() + separator);
        let mut keep = room.min(line.len());
        while !line.is_char_boundary(keep) {
            keep -= 1;
        }
        if keep > 0 {
            if separator == 1 {
                self.text.push('\n');
            }
            self.text.push_str(&line[..keep]);
        }
        self.dropped = line.len() - keep;
    }

    /// `output` followed by the captured console, when there is any.
    fn attach(&self, output: String) -> String {
        if self.text.is_empty() && self.dropped == 0 {
            return output;
        }
        let mut attached = format!("{output}\nconsole:\n{}", self.text);
        if self.dropped > 0 {
            attached.push_str(&format!(
                "\n… console truncated: {} more bytes",
                self.dropped
            ));
        }
        attached
    }
}

/// Tickets issued, calls in flight and completions buffered during a sync helper.
#[derive(Default)]
struct Bridge {
    next_ticket: Counter<u32>,
    in_flight: Counter<usize>,
    queued_completions: RefCell<VecDeque<(u32, Result<Payload, String>)>>,
}

/// JavaScript execution time, without the waits for the host.
struct CpuClock {
    limit: Duration,
    spent: Counter<Duration>,
    since: Counter<Option<Instant>>,
}

impl CpuClock {
    fn new(limit: Duration) -> Self {
        Self {
            limit,
            spent: Counter::new(Duration::ZERO),
            since: Counter::new(Some(Instant::now())),
        }
    }

    fn exceeded(&self) -> bool {
        self.exceeded_at(Instant::now())
    }

    fn exceeded_at(&self, now: Instant) -> bool {
        let running = self
            .since
            .get()
            .map_or(Duration::ZERO, |since| now.saturating_duration_since(since));
        self.spent.get() + running >= self.limit
    }

    /// Runs `wait` with the clock stopped.
    fn paused<T>(&self, wait: impl FnOnce() -> T) -> T {
        if let Some(since) = self.since.take() {
            self.spent.set(self.spent.get() + since.elapsed());
        }
        let result = wait();
        self.since.set(Some(Instant::now()));
        result
    }
}

pub(super) fn spawn(
    code: String,
    run: CancellationToken,
    limits: Limits,
    natives: Vec<String>,
) -> Cell {
    let (sender, requests) = channel::<Request>(REQUEST_CHANNEL_CAPACITY);
    let (wake, wake_rx) = mpsc::channel::<WorkerWake>();
    let cancellation = CancellationToken::new();
    let deadline = Instant::now() + limits.wall;
    watch(run.clone(), cancellation.clone(), deadline, wake.clone());
    let stop = cancellation.clone();
    let pool = Arc::clone(&WORKER_POOL);
    let reserved = pool
        .busy
        .try_update(Ordering::AcqRel, Ordering::Acquire, |cur| {
            (cur < DEDICATED_JS_WORKERS).then_some(cur + 1)
        })
        .is_ok();
    let worker = if reserved {
        let (reply_tx, reply_rx) = oneshot::channel::<Output>();
        let job = Box::new(CellJob {
            code,
            run: run.clone(),
            stop,
            deadline,
            cpu: limits.cpu,
            sender,
            natives,
            wake_rx,
            reply: reply_tx,
        });
        {
            let mut q = pool
                .queue
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            q.push_back(WorkerMsg::Run(job));
        }
        pool.cvar.notify_one();
        let work = run.track_native_work();
        tokio::spawn(async move {
            let _work = work;
            reply_rx
                .await
                .unwrap_or_else(|_| Output::failed("QuickJS worker terminated unexpectedly".into()))
        })
    } else {
        let work = run.track_native_work();
        tokio::task::spawn_blocking(move || {
            let _work = work;
            let console = Rc::new(RefCell::new(Console::default()));
            let mut slot_rt = WARM_RUNTIME.with(|slot| slot.borrow_mut().take());
            let mut slot_ctx = None;
            let output = interpret_with_slot(
                &mut slot_rt,
                &mut slot_ctx,
                &code,
                &run,
                &stop,
                deadline,
                limits.cpu,
                sender,
                &natives,
                &console,
                wake_rx,
                None,
            )
            .expect("interpret_with_slot returns output when no reply sender");
            drop(slot_ctx);
            if let Some(rt) = slot_rt {
                WARM_RUNTIME.with(|slot| {
                    let mut s = slot.borrow_mut();
                    if s.is_none() {
                        *s = Some(rt);
                    }
                });
            }
            output
        })
    };
    Cell {
        requests,
        worker,
        cancellation,
        deadline,
        wake,
    }
}

fn instantiate_prelude<'js>(ctx: &Ctx<'js>) -> rquickjs::Result<Function<'js>> {
    if let Some(bytecode) = PRELUDE_BYTECODE.as_deref() {
        if let Ok(module) = unsafe { Module::load(ctx.clone(), bytecode) } {
            if let Ok((evaluated, _promise)) = module.eval() {
                if let Ok(func) = evaluated.get::<_, Function>("default") {
                    return Ok(func);
                }
            }
        }
    }
    ctx.eval(PRELUDE)
}

fn send_or_return(reply: Option<oneshot::Sender<Output>>, out: Output) -> Option<Output> {
    if let Some(tx) = reply {
        let _ = tx.send(out);
        None
    } else {
        Some(out)
    }
}

/// Runs one program to its end, with every call it started settled.
#[allow(clippy::too_many_arguments)]
fn interpret_with_slot(
    warm_rt: &mut Option<rquickjs::Runtime>,
    warm_ctx: &mut Option<Context>,
    code: &str,
    run: &CancellationToken,
    stop: &CancellationToken,
    deadline: Instant,
    cpu: Duration,
    sender: tokio::sync::mpsc::Sender<Request>,
    natives: &[String],
    console: &Rc<RefCell<Console>>,
    wake_rx: mpsc::Receiver<WorkerWake>,
    reply: Option<oneshot::Sender<Output>>,
) -> Option<Output> {
    if code.len() > MAX_CODE_BYTES {
        return send_or_return(
            reply,
            Output {
                result: Err("code exceeds 64 KiB".into()),
                console: console.take(),
                unhandled_rejections: Vec::new(),
            },
        );
    }
    let runtime = match warm_rt.take().map_or_else(rquickjs::Runtime::new, Ok) {
        Ok(runtime) => runtime,
        Err(error) => {
            *warm_ctx = None;
            return send_or_return(
                reply,
                Output {
                    result: Err(error.to_string()),
                    console: console.take(),
                    unhandled_rejections: Vec::new(),
                },
            );
        }
    };
    configure_runtime(&runtime);
    let clock = Rc::new(CpuClock::new(cpu));
    let interrupted = {
        let (run, stop, clock) = (run.clone(), stop.clone(), Rc::clone(&clock));
        move || {
            if run.is_cancelled() || stop.is_cancelled() {
                return true;
            }
            let now = Instant::now();
            now >= deadline || clock.exceeded_at(now)
        }
    };
    runtime.set_interrupt_handler(Some(Box::new(interrupted.clone())));
    let unhandled_rejections: Rc<RefCell<Vec<(Persistent<rquickjs::Value<'static>>, String)>>> =
        Rc::new(RefCell::new(Vec::new()));
    {
        let rejections = Rc::clone(&unhandled_rejections);
        runtime.set_host_promise_rejection_tracker(Some(Box::new(
            move |ctx, promise, reason, is_handled| {
                let key = Persistent::save(&ctx, promise);
                let mut list = rejections.borrow_mut();
                if is_handled {
                    list.retain(|(existing, _)| existing != &key);
                } else if list.len() < MAX_UNHANDLED_REJECTIONS
                    && !list.iter().any(|(existing, _)| existing == &key)
                {
                    list.push((key, format_js_rejection(&ctx, reason)));
                }
            },
        )));
    }
    let context = match warm_ctx.take().map_or_else(|| Context::full(&runtime), Ok) {
        Ok(context) => context,
        Err(error) => {
            runtime.set_host_promise_rejection_tracker(None);
            runtime.set_interrupt_handler(None);
            return send_or_return(
                reply,
                Output {
                    result: Err(error.to_string()),
                    console: console.take(),
                    unhandled_rejections: Vec::new(),
                },
            );
        }
    };
    let wake_rx = Rc::new(wake_rx);
    let (result, rejections) = context.with(|ctx| {
        let caught = |error: rquickjs::Error| CaughtError::from_error(&ctx, error).to_string();
        let bridge = Rc::new(Bridge::default());
        let loop_ = EventLoop {
            ctx: &ctx,
            bridge: &bridge,
            clock: &clock,
            wake_rx: &wake_rx,
            interrupted: &interrupted,
        };
        let returned = sender.clone();
        let settle = match (|| -> rquickjs::Result<Function> {
            let call_fn = call_function(
                &ctx,
                sender.clone(),
                Rc::clone(&bridge),
                Rc::clone(&clock),
                interrupted.clone(),
            )?;
            let helper_fn = helper_function(
                &ctx,
                sender,
                Rc::clone(&bridge),
                Rc::clone(&clock),
                Rc::clone(&wake_rx),
                interrupted.clone(),
            )?;
            let sink = Rc::clone(console);
            let log_fn = Function::new(ctx.clone(), move |line: String| {
                sink.borrow_mut().push(&line)
            })?;
            let grep_fn = native_grep_text_fn(&ctx)?;
            let diff_fn = native_diff_text_fn(&ctx)?;
            let natives_js = Array::new(ctx.clone())?;
            for (idx, name) in natives.iter().enumerate() {
                natives_js.set(idx, name.as_str())?;
            }
            let init = instantiate_prelude(&ctx)?;
            init.call((call_fn, helper_fn, log_fn, natives_js, grep_fn, diff_fn))
        })()
        .map_err(caught)
        {
            Ok(settle) => settle,
            Err(error) => {
                std::mem::take(&mut *unhandled_rejections.borrow_mut());
                return (Err(error), Vec::new());
            }
        };
        let eval_fn = match ctx.eval::<Function, _>(format!(
            "(async () => {{\"use strict\";\n{code}\n}})"
        )) {
            Ok(func) => Ok(func),
            Err(err) => {
                let first_error = caught(err);
                let stripped = strip_typescript_annotations(code);
                if stripped != code {
                    ctx.eval::<Function, _>(format!(
                        "(async () => {{\"use strict\";\n{stripped}\n}})"
                    ))
                    .map_err(&caught)
                } else {
                    Err(first_error)
                }
            }
        };
        let eval_promise =
            eval_fn.and_then(|func| func.call::<_, Promise>(()).map_err(&caught));
        let root_promise = eval_promise
            .as_ref()
            .ok()
            .map(|promise| Persistent::save(&ctx, promise.clone().into_value()));
        let mut settled = eval_promise
            .and_then(|promise| loop_.settle_promise(&promise, &settle).map_err(&caught))
            .and_then(|value| result_json(&ctx, value).map_err(&caught));
        // The program ended; the calls it did not await still run to the end.
        if loop_.bridge.in_flight.get() > 0 && !interrupted() {
            let _ = clock.paused(|| returned.blocking_send(Request::Returned));
            if let Err(error) = loop_.drain(&settle).map_err(caught) {
                if settled.is_ok() {
                    settled = Err(error);
                }
            }
        }
        while !interrupted() && ctx.execute_pending_job() {}
        let mut raw_rejections = std::mem::take(&mut *unhandled_rejections.borrow_mut());
        if let Some(root) = root_promise {
            raw_rejections.retain(|(existing, _)| existing != &root);
            drop(root);
        }
        let rejections = raw_rejections
            .into_iter()
            .map(|(_, text)| text)
            .collect::<Vec<_>>();
        (settled, rejections)
    });
    runtime.set_host_promise_rejection_tracker(None);
    runtime.set_interrupt_handler(None);
    let was_interrupted = interrupted();
    let cpu_exceeded = clock.exceeded();
    let final_result = if cpu_exceeded {
        Err(format!(
            "JavaScript CPU time limit of {} s exceeded (waits for tool calls do not count)",
            cpu.as_secs()
        ))
    } else {
        result
    };
    let is_pool_worker = reply.is_some();
    let reply_out = send_or_return(
        reply,
        Output {
            result: final_result,
            console: console.take(),
            unhandled_rejections: rejections,
        },
    );
    drop(context);
    if !was_interrupted && !cpu_exceeded && !runtime.is_job_pending() {
        runtime.run_gc();
        if is_pool_worker {
            *warm_ctx = Context::full(&runtime).ok();
        }
        *warm_rt = Some(runtime);
    }
    reply_out
}

fn split_js_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        Vec::new()
    } else {
        text.split('\n')
            .map(|line| line.strip_suffix('\r').unwrap_or(line))
            .collect()
    }
}

fn grep_text_impl<'js>(
    ctx: Ctx<'js>,
    text: String,
    pattern: String,
    ignore_case: bool,
    context: i32,
) -> rquickjs::Result<Array<'js>> {
    let raw = split_js_lines(&text);
    let ctx_lines = usize::try_from(context.max(0)).unwrap_or(0);
    let ascii_ci = ignore_case && pattern.is_ascii();
    let needle_lower = ignore_case.then(|| {
        if ascii_ci {
            pattern.to_ascii_lowercase()
        } else {
            pattern.to_lowercase()
        }
    });
    let out = Array::new(ctx.clone())?;
    let mut out_idx = 0usize;
    for (i, &line) in raw.iter().enumerate() {
        let matched = match (&needle_lower, ascii_ci) {
            (Some(lower), true) => {
                super::dispatch::contains_ascii_ci(line.as_bytes(), lower.as_bytes())
            }
            (Some(lower), false) => line.to_lowercase().contains(lower.as_str()),
            (None, _) => line.contains(pattern.as_str()),
        };
        if !matched {
            continue;
        }
        let entry = Object::new(ctx.clone())?;
        entry.set("line", i + 1)?;
        entry.set("text", line)?;
        let context_arr = Array::new(ctx.clone())?;
        if ctx_lines > 0 {
            let start = i.saturating_sub(ctx_lines);
            let end = (i + ctx_lines).min(raw.len().saturating_sub(1));
            let mut cidx = 0usize;
            for (j, &cline) in raw.iter().enumerate().take(end + 1).skip(start) {
                if j != i {
                    let cobj = Object::new(ctx.clone())?;
                    cobj.set("line", j + 1)?;
                    cobj.set("text", cline)?;
                    context_arr.set(cidx, cobj)?;
                    cidx += 1;
                }
            }
        }
        entry.set("context", context_arr)?;
        out.set(out_idx, entry)?;
        out_idx += 1;
    }
    Ok(out)
}

fn native_grep_text_fn<'js>(ctx: &Ctx<'js>) -> rquickjs::Result<Function<'js>> {
    Function::new(ctx.clone(), grep_text_impl)
}

fn native_diff_text_fn<'js>(ctx: &Ctx<'js>) -> rquickjs::Result<Function<'js>> {
    use std::fmt::Write as _;
    Function::new(
        ctx.clone(),
        |before: String, after: String, context: i32| -> String {
            let a = split_js_lines(&before);
            let b = split_js_lines(&after);
            let mut p = 0usize;
            while p < a.len() && p < b.len() && a[p] == b[p] {
                p += 1;
            }
            let mut s = 0usize;
            while s < a.len() - p
                && s < b.len() - p
                && a[a.len() - 1 - s] == b[b.len() - 1 - s]
            {
                s += 1;
            }
            let a_mid = &a[p..a.len() - s];
            let b_mid = &b[p..b.len() - s];
            if a_mid.is_empty() && b_mid.is_empty() {
                return String::new();
            }
            let ctx_n = usize::try_from(context.max(0)).unwrap_or(0);
            let mut out = String::new();
            if a_mid.len().saturating_mul(b_mid.len()) > 40_000 {
                let _ = write!(
                    out,
                    "@@ -{},{} +{},{} @@",
                    p + 1,
                    a_mid.len(),
                    p + 1,
                    b_mid.len()
                );
                for &ln in &a[p.saturating_sub(ctx_n)..p] {
                    let _ = write!(out, "\n {ln}");
                }
                for &ln in a_mid {
                    let _ = write!(out, "\n-{ln}");
                }
                for &ln in b_mid {
                    let _ = write!(out, "\n+{ln}");
                }
                let s_end = (a.len() - s + ctx_n).min(a.len());
                for &ln in &a[a.len() - s..s_end] {
                    let _ = write!(out, "\n {ln}");
                }
                return out;
            }
            let (m, n) = (a_mid.len(), b_mid.len());
            let stride = n + 1;
            let mut dp = vec![0u32; (m + 1) * stride];
            for i in (0..m).rev() {
                for j in (0..n).rev() {
                    dp[i * stride + j] = if a_mid[i] == b_mid[j] {
                        dp[(i + 1) * stride + (j + 1)] + 1
                    } else {
                        dp[(i + 1) * stride + j].max(dp[i * stride + (j + 1)])
                    };
                }
            }
            let mut ops: Vec<(char, &str)> = Vec::with_capacity(m + n);
            let (mut i, mut j) = (0usize, 0usize);
            while i < m && j < n {
                if a_mid[i] == b_mid[j] {
                    ops.push((' ', a_mid[i]));
                    i += 1;
                    j += 1;
                } else if dp[(i + 1) * stride + j] >= dp[i * stride + (j + 1)] {
                    ops.push(('-', a_mid[i]));
                    i += 1;
                } else {
                    ops.push(('+', b_mid[j]));
                    j += 1;
                }
            }
            while i < m {
                ops.push(('-', a_mid[i]));
                i += 1;
            }
            while j < n {
                ops.push(('+', b_mid[j]));
                j += 1;
            }
            let pre_start = p.saturating_sub(ctx_n);
            let pre_len = p - pre_start;
            let suf_len = s.min(ctx_n);
            let _ = write!(
                out,
                "@@ -{},{} +{},{} @@",
                pre_start + 1,
                a_mid.len() + pre_len + suf_len,
                pre_start + 1,
                b_mid.len() + pre_len + suf_len
            );
            for &ln in &a[pre_start..p] {
                let _ = write!(out, "\n {ln}");
            }
            for (tag, ln) in ops {
                let _ = write!(out, "\n{tag}{ln}");
            }
            for &ln in &a[a.len() - s..a.len() - s + suf_len] {
                let _ = write!(out, "\n {ln}");
            }
            out
        },
    )
}

fn format_js_rejection<'js>(ctx: &Ctx<'js>, reason: rquickjs::Value<'js>) -> String {
    let text = if let Some(exc) = reason.as_exception() {
        exc.to_string()
    } else if let Some(text) = reason.as_string().and_then(|s| s.to_string().ok()) {
        text
    } else if let Ok(Some(json)) = ctx.json_stringify(reason.clone()) {
        json.to_string().unwrap_or_else(|_| "rejected".into())
    } else {
        format!("{:?}", reason.type_of())
    };
    text.lines().next().unwrap_or("rejected").chars().take(200).collect()
}

fn result_json<'js>(ctx: &Ctx<'js>, value: rquickjs::Value<'js>) -> rquickjs::Result<String> {
    let output = ctx
        .json_stringify(value)?
        .map(|text| text.to_string())
        .transpose()?
        .unwrap_or_else(|| "null".into());
    if output.len() > MAX_JSON_BYTES {
        return Err(Exception::throw_message(
            ctx,
            "cell result exceeds 1 MiB; return a smaller summary",
        ));
    }
    Ok(output)
}

/// Cancels the cell when the run is cancelled or the deadline passes, and wakes
/// the blocked worker thread immediately.
fn watch(
    run: CancellationToken,
    cell: CancellationToken,
    deadline: Instant,
    wake: mpsc::Sender<WorkerWake>,
) {
    tokio::spawn(async move {
        tokio::select! {
            () = cell.cancelled() => {}
            () = run.cancelled() => cell.cancel(),
            () = tokio::time::sleep_until(deadline.into()) => cell.cancel(),
        }
        let _ = wake.send(WorkerWake::Interrupt);
    });
}

/// Starts an async `tools.call` on the host and returns its ticket immediately.
fn call_function<'js>(
    ctx: &Ctx<'js>,
    sender: tokio::sync::mpsc::Sender<Request>,
    bridge: Rc<Bridge>,
    clock: Rc<CpuClock>,
    interrupted: impl Fn() -> bool + 'static,
) -> rquickjs::Result<Function<'js>> {
    Function::new(ctx.clone(), move |ctx: Ctx<'_>, name: String, raw_arguments: String| {
        let dispatch = || -> Result<u32, String> {
            if name.len().saturating_add(raw_arguments.len()) > MAX_JSON_BYTES {
                return Err("host request exceeds 1 MiB".into());
            }
            if interrupted() {
                return Err(INTERRUPTED.into());
            }
            let disconnected = |_| HOST_DISCONNECTED.to_owned();
            let arguments: Value =
                serde_json::from_str(&raw_arguments).map_err(|error| error.to_string())?;
            let ticket = bridge.next_ticket.get();
            bridge.next_ticket.set(ticket.wrapping_add(1));
            clock
                .paused(|| {
                    sender.blocking_send(Request::Call {
                        ticket,
                        name,
                        arguments,
                        raw_arguments,
                    })
                })
                .map_err(disconnected)?;
            bridge.in_flight.set(bridge.in_flight.get() + 1);
            Ok(ticket)
        };
        dispatch().map_err(|error| Exception::throw_message(&ctx, &error))
    })
}

/// Runs a synchronous helper and blocks until the host replies without polling.
fn helper_function<'js>(
    ctx: &Ctx<'js>,
    sender: tokio::sync::mpsc::Sender<Request>,
    bridge: Rc<Bridge>,
    clock: Rc<CpuClock>,
    wake_rx: Rc<mpsc::Receiver<WorkerWake>>,
    interrupted: impl Fn() -> bool + 'static,
) -> rquickjs::Result<Function<'js>> {
    Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>, input: String| -> rquickjs::Result<rquickjs::Value<'js>> {
        let dispatch = || -> Result<Value, String> {
            if input.len() > MAX_JSON_BYTES {
                return Err("host request exceeds 1 MiB".into());
            }
            if interrupted() {
                return Err(INTERRUPTED.into());
            }
            let disconnected = |_| HOST_DISCONNECTED.to_owned();
            let command: Command =
                serde_json::from_str(&input).map_err(|error| error.to_string())?;
            clock.paused(|| {
                sender
                    .blocking_send(Request::Helper { command })
                    .map_err(disconnected)?;
                loop {
                    if interrupted() {
                        return Err(INTERRUPTED.into());
                    }
                    match wake_rx.recv() {
                        Ok(WorkerWake::HelperReply(result)) => return result,
                        Ok(WorkerWake::Completion { ticket, result }) => {
                            bridge
                                .queued_completions
                                .borrow_mut()
                                .push_back((ticket, result));
                        }
                        Ok(WorkerWake::Interrupt) => {}
                        Err(_) => return Err(HOST_DISCONNECTED.into()),
                    }
                }
            })
        };
        match dispatch() {
            Ok(val) => json_value_to_js(&ctx, &val, 0),
            Err(error) => Err(Exception::throw_message(&ctx, &error)),
        }
    })
}

/// The worker's event loop over the program's promises.
struct EventLoop<'a, 'js> {
    ctx: &'a Ctx<'js>,
    bridge: &'a Bridge,
    clock: &'a CpuClock,
    wake_rx: &'a mpsc::Receiver<WorkerWake>,
    interrupted: &'a dyn Fn() -> bool,
}

impl<'js> EventLoop<'_, 'js> {
    /// Runs pending jobs until `promise` settles, waiting for call
    /// completions in between. A pending promise with no call in flight can
    /// never settle and fails as a deadlock.
    fn settle_promise(
        &self,
        promise: &Promise<'js>,
        settle: &Function<'js>,
    ) -> rquickjs::Result<rquickjs::Value<'js>> {
        loop {
            match promise.finish::<rquickjs::Value>() {
                Err(rquickjs::Error::WouldBlock) => {}
                settled => return settled,
            }
            if self.bridge.in_flight.get() == 0 {
                return Err(Exception::throw_message(self.ctx, DEADLOCK));
            }
            self.next_completions(settle)?;
        }
    }

    /// Settles every call still in flight after the program returned, and
    /// whatever those settlements start.
    fn drain(&self, settle: &Function<'js>) -> rquickjs::Result<()> {
        while self.bridge.in_flight.get() > 0 {
            if (self.interrupted)() {
                return Err(Exception::throw_message(self.ctx, INTERRUPTED));
            }
            self.next_completions(settle)?;
            while !(self.interrupted)() && self.ctx.execute_pending_job() {}
        }
        Ok(())
    }

    /// Waits for the next completion and settles every one available.
    fn next_completions(&self, settle: &Function<'js>) -> rquickjs::Result<()> {
        if (self.interrupted)() {
            return Err(Exception::throw_message(self.ctx, INTERRUPTED));
        }
        if let Some((ticket, result)) = self.bridge.queued_completions.borrow_mut().pop_front() {
            settle_completion(settle, self.bridge, ticket, result)?;
            while let Some((ticket, result)) =
                self.bridge.queued_completions.borrow_mut().pop_front()
            {
                if (self.interrupted)() {
                    return Err(Exception::throw_message(self.ctx, INTERRUPTED));
                }
                settle_completion(settle, self.bridge, ticket, result)?;
            }
            return self.drain_ready_wakes(settle);
        }
        let completion = self.clock.paused(|| loop {
            if (self.interrupted)() {
                return Err(INTERRUPTED);
            }
            match self.wake_rx.recv() {
                Ok(WorkerWake::Completion { ticket, result }) => return Ok((ticket, result)),
                Ok(WorkerWake::HelperReply(_) | WorkerWake::Interrupt) => {}
                Err(_) => return Err(HOST_DISCONNECTED),
            }
        });
        let (ticket, result) =
            completion.map_err(|error| Exception::throw_message(self.ctx, error))?;
        settle_completion(settle, self.bridge, ticket, result)?;
        self.drain_ready_wakes(settle)
    }

    fn drain_ready_wakes(&self, settle: &Function<'js>) -> rquickjs::Result<()> {
        while let Ok(wake) = self.wake_rx.try_recv() {
            match wake {
                WorkerWake::Completion { ticket, result } => {
                    if (self.interrupted)() {
                        return Err(Exception::throw_message(self.ctx, INTERRUPTED));
                    }
                    settle_completion(settle, self.bridge, ticket, result)?;
                }
                WorkerWake::Interrupt => {
                    if (self.interrupted)() {
                        return Err(Exception::throw_message(self.ctx, INTERRUPTED));
                    }
                }
                WorkerWake::HelperReply(_) => {}
            }
        }
        Ok(())
    }
}

fn json_value_to_js<'js>(
    ctx: &Ctx<'js>,
    val: &Value,
    depth: usize,
) -> rquickjs::Result<rquickjs::Value<'js>> {
    if depth > 256 {
        return Err(Exception::throw_message(
            ctx,
            "structured JSON nesting exceeds max depth",
        ));
    }
    match val {
        Value::Null => Ok(rquickjs::Value::new_null(ctx.clone())),
        Value::Bool(b) => Ok(rquickjs::Value::new_bool(ctx.clone(), *b)),
        Value::Number(n) => {
            if let Some(i) = n.as_i64().and_then(|v| i32::try_from(v).ok()) {
                Ok(rquickjs::Value::new_int(ctx.clone(), i))
            } else if let Some(f) = n.as_f64() {
                Ok(rquickjs::Value::new_float(ctx.clone(), f))
            } else {
                Ok(rquickjs::Value::new_null(ctx.clone()))
            }
        }
        Value::String(s) => rquickjs::String::from_str(ctx.clone(), s).map(|js| js.into_value()),
        Value::Array(items) => {
            let arr = Array::new(ctx.clone())?;
            for (idx, item) in items.iter().enumerate() {
                arr.set(idx, json_value_to_js(ctx, item, depth + 1)?)?;
            }
            Ok(arr.into_value())
        }
        Value::Object(map) => {
            let obj = Object::new(ctx.clone())?;
            for (k, v) in map {
                obj.set(k.as_str(), json_value_to_js(ctx, v, depth + 1)?)?;
            }
            Ok(obj.into_value())
        }
    }
}

fn settle_completion<'js>(
    settle: &Function<'js>,
    bridge: &Bridge,
    ticket: u32,
    result: Result<Payload, String>,
) -> rquickjs::Result<()> {
    bridge
        .in_flight
        .set(bridge.in_flight.get().saturating_sub(1));
    let ctx = settle.ctx();
    let (kind, payload): (u8, rquickjs::Value<'js>) = match result {
        Ok(Payload::Text(text)) => (
            1u8,
            rquickjs::String::from_str(ctx.clone(), &text)?.into_value(),
        ),
        Ok(Payload::McpJson(json)) => (
            2u8,
            rquickjs::String::from_str(ctx.clone(), &json)?.into_value(),
        ),
        Ok(Payload::Json(json)) => (
            3u8,
            rquickjs::String::from_str(ctx.clone(), &json)?.into_value(),
        ),
        Ok(Payload::JsonValue(value)) => (4u8, json_value_to_js(ctx, &value, 0)?),
        Err(error) => (
            0u8,
            rquickjs::String::from_str(ctx.clone(), &error)?.into_value(),
        ),
    };
    settle.call::<_, ()>((ticket, kind, payload))
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || matches!(b, b'_' | b'$')
}

fn is_ident_cont(b: u8) -> bool {
    is_ident_start(b) || b.is_ascii_digit()
}

fn skip_ws_inline(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && matches!(b[i], b' ' | b'\t') {
        i += 1;
    }
    i
}

fn skip_ws_and_comments(b: &[u8], mut i: usize) -> usize {
    while i < b.len() {
        if b[i].is_ascii_whitespace() {
            i += 1;
        } else if i + 1 < b.len() && b[i] == b'/' && b[i + 1] == b'/' {
            i += 2;
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if i + 1 < b.len() && b[i] == b'/' && b[i + 1] == b'*' {
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(b.len());
        } else {
            break;
        }
    }
    i
}

fn scan_string_or_comment(b: &[u8], i: usize) -> Option<usize> {
    if i >= b.len() {
        return None;
    }
    if matches!(b[i], b'\'' | b'"' | b'`') {
        let quote = b[i];
        let mut j = i + 1;
        while j < b.len() {
            if b[j] == b'\\' {
                j = (j + 2).min(b.len());
                continue;
            }
            if b[j] == quote {
                return Some(j + 1);
            }
            j += 1;
        }
        return Some(b.len());
    }
    if i + 1 < b.len() && b[i] == b'/' && b[i + 1] == b'/' {
        let mut j = i + 2;
        while j < b.len() && b[j] != b'\n' {
            j += 1;
        }
        return Some(j);
    }
    if i + 1 < b.len() && b[i] == b'/' && b[i + 1] == b'*' {
        let mut j = i + 2;
        while j + 1 < b.len() && !(b[j] == b'*' && b[j + 1] == b'/') {
            j += 1;
        }
        return Some((j + 2).min(b.len()));
    }
    None
}

fn scan_balanced(b: &[u8], start: usize, open: u8, close: u8) -> usize {
    if start >= b.len() || b[start] != open {
        return start;
    }
    let mut depth = 0usize;
    let mut i = start;
    while i < b.len() {
        if let Some(next) = scan_string_or_comment(b, i) {
            i = next;
            continue;
        }
        if b[i] == open {
            depth += 1;
        } else if b[i] == close {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                return i + 1;
            }
        }
        i += 1;
    }
    b.len()
}

fn read_ident(b: &[u8], start: usize) -> Option<(&str, usize)> {
    if start >= b.len() || !is_ident_start(b[start]) {
        return None;
    }
    let mut end = start + 1;
    while end < b.len() && is_ident_cont(b[end]) {
        end += 1;
    }
    std::str::from_utf8(&b[start..end]).ok().map(|s| (s, end))
}

fn skip_ts_type(b: &[u8], start: usize) -> usize {
    let mut i = skip_ws_and_comments(b, start);
    let mut saw_fn_params = false;
    while i < b.len() {
        if let Some(next) = scan_string_or_comment(b, i) {
            if matches!(b[i], b'\'' | b'"' | b'`') {
                saw_fn_params = false;
                i = next;
                continue;
            }
            break;
        }
        match b[i] {
            b'<' | b'(' | b'[' | b'{' => {
                let open = b[i];
                let close = match open {
                    b'<' => b'>',
                    b'(' => b')',
                    b'[' => b']',
                    _ => b'}',
                };
                saw_fn_params = open == b'(';
                i = scan_balanced(b, i, open, close);
            }
            b'|' | b'&' | b'.' | b'?' => {
                saw_fn_params = false;
                i = skip_ws_and_comments(b, i + 1);
            }
            b'=' if saw_fn_params && i + 1 < b.len() && b[i + 1] == b'>' => {
                saw_fn_params = false;
                i = skip_ws_and_comments(b, i + 2);
            }
            _ if is_ident_start(b[i]) => {
                saw_fn_params = false;
                let (word, end) = read_ident(b, i).unwrap_or(("", i + 1));
                let after = skip_ws_inline(b, end);
                if matches!(word, "readonly" | "keyof" | "typeof" | "infer" | "is" | "asserts") {
                    i = after;
                    continue;
                }
                if after < b.len() && b[after] == b'<' {
                    i = scan_balanced(b, after, b'<', b'>');
                } else {
                    i = end;
                }
            }
            _ if b[i].is_ascii_digit() => {
                saw_fn_params = false;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'.') {
                    i += 1;
                }
            }
            b' ' | b'\t' => {
                let next = skip_ws_inline(b, i);
                if next < b.len() && matches!(b[next], b'|' | b'&' | b'[' | b'<' | b'.') {
                    i = next;
                } else if saw_fn_params
                    && next + 1 < b.len()
                    && b[next] == b'='
                    && b[next + 1] == b'>'
                {
                    i = next;
                } else {
                    break;
                }
            }
            _ => break,
        }
    }
    i
}

fn at_statement_start(out: &str) -> bool {
    let trimmed = out.trim_end_matches([' ', '\t']);
    trimmed.is_empty()
        || trimmed.ends_with('\n')
        || trimmed.ends_with(';')
        || trimmed.ends_with('{')
        || trimmed.ends_with('}')
}

fn prev_word(out: &str) -> &str {
    let trimmed = out.trim_end();
    let start = trimmed
        .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '$'))
        .map_or(0, |idx| idx + 1);
    &trimmed[start..]
}

fn prev_non_ws_byte(out: &str) -> Option<u8> {
    out.bytes().rev().find(|b| !b.is_ascii_whitespace())
}

fn strip_param_list(inner: &str) -> String {
    let b = inner.as_bytes();
    let mut out = String::with_capacity(inner.len());
    let mut i = 0usize;
    let mut param_start = true;
    while i < b.len() {
        if let Some(next) = scan_string_or_comment(b, i) {
            out.push_str(&inner[i..next]);
            i = next;
            param_start = false;
            continue;
        }
        if b[i] == b',' {
            out.push(',');
            i += 1;
            param_start = true;
            continue;
        }
        if param_start {
            let ws_end = skip_ws_and_comments(b, i);
            if ws_end > i {
                out.push_str(&inner[i..ws_end]);
                i = ws_end;
                continue;
            }
            if let Some((word, end)) = read_ident(b, i) {
                if matches!(word, "readonly" | "public" | "private" | "protected")
                    && end < b.len()
                    && b[end].is_ascii_whitespace()
                {
                    i = skip_ws_inline(b, end);
                    continue;
                }
            }
            param_start = false;
        }
        if b[i] == b'?' {
            let after = skip_ws_and_comments(b, i + 1);
            if after < b.len() && (b[after] == b':' || b[after] == b',' || b[after] == b'=') {
                i = after;
                continue;
            }
        }
        if b[i] == b':' {
            i = skip_ts_type(b, i + 1);
            continue;
        }
        if matches!(b[i], b'{' | b'[' | b'(') {
            let close = match b[i] {
                b'{' => b'}',
                b'[' => b']',
                _ => b')',
            };
            let end = scan_balanced(b, i, b[i], close);
            out.push_str(&inner[i..end]);
            i = end;
            continue;
        }
        let ch = inner[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

pub(super) fn strip_typescript_annotations(code: &str) -> String {
    let b = code.as_bytes();
    let mut out = String::with_capacity(code.len());
    let mut i = 0usize;
    while i < b.len() {
        if let Some(next) = scan_string_or_comment(b, i) {
            out.push_str(&code[i..next]);
            i = next;
            continue;
        }
        if at_statement_start(&out) {
            let mut scan = i;
            if let Some(("export", after_exp)) = read_ident(b, scan) {
                if after_exp < b.len() && b[after_exp].is_ascii_whitespace() {
                    scan = skip_ws_inline(b, after_exp);
                }
            }
            if let Some((kw, after_kw)) = read_ident(b, scan) {
                if kw == "interface" && after_kw < b.len() && b[after_kw].is_ascii_whitespace() {
                    let mut j = skip_ws_and_comments(b, after_kw);
                    while j < b.len() && b[j] != b'{' {
                        j += 1;
                    }
                    if j < b.len() && b[j] == b'{' {
                        j = scan_balanced(b, j, b'{', b'}');
                        let tail = skip_ws_inline(b, j);
                        i = if tail < b.len() && b[tail] == b';' {
                            tail + 1
                        } else {
                            j
                        };
                        continue;
                    }
                } else if kw == "type" && after_kw < b.len() && b[after_kw].is_ascii_whitespace() {
                    let mut j = skip_ws_and_comments(b, after_kw);
                    if let Some((_, after_name)) = read_ident(b, j) {
                        j = skip_ws_and_comments(b, after_name);
                        if j < b.len() && b[j] == b'<' {
                            j = scan_balanced(b, j, b'<', b'>');
                            j = skip_ws_and_comments(b, j);
                        }
                        if j < b.len() && b[j] == b'=' && !(j + 1 < b.len() && b[j + 1] == b'=') {
                            j += 1;
                            let mut depth = 0usize;
                            while j < b.len() {
                                if let Some(next) = scan_string_or_comment(b, j) {
                                    j = next;
                                    continue;
                                }
                                match b[j] {
                                    b'(' | b'[' | b'{' | b'<' => depth += 1,
                                    b')' | b']' | b'}' | b'>' => depth = depth.saturating_sub(1),
                                    b';' if depth == 0 => {
                                        j += 1;
                                        break;
                                    }
                                    b'\n' if depth == 0 => break,
                                    _ => {}
                                }
                                j += 1;
                            }
                            i = j;
                            continue;
                        }
                    }
                }
            }
        }
        if b[i] == b'<' {
            let without_name = out.trim_end();
            let name_start = without_name
                .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '$'))
                .map_or(0, |p| p + 1);
            let is_fn_generic = prev_word(&out) == "function"
                || (name_start < without_name.len()
                    && prev_word(&without_name[..name_start]) == "function");
            if is_fn_generic {
                let close = scan_balanced(b, i, b'<', b'>');
                let after = skip_ws_and_comments(b, close);
                if after < b.len() && b[after] == b'(' {
                    i = close;
                    continue;
                }
            }
        }
        if b[i] == b'!'
            && b.get(i + 1) != Some(&b'=')
            && prev_non_ws_byte(&out)
                .is_some_and(|pb| is_ident_cont(pb) || matches!(pb, b')' | b']'))
            && matches!(
                b.get(i + 1),
                Some(b'.' | b'[' | b'(' | b',' | b';' | b')' | b']' | b'}' | b' ' | b'\t' | b'\r' | b'\n')
                    | None
            )
        {
            i += 1;
            continue;
        }
        if b[i] == b'(' {
            let close = scan_balanced(b, i, b'(', b')');
            let after_close = skip_ws_and_comments(b, close);
            let fn_context = prev_word(&out) == "function"
                || (after_close + 1 < b.len()
                    && b[after_close] == b'='
                    && b[after_close + 1] == b'>')
                || (after_close < b.len() && b[after_close] == b':');
            let return_type_end = if fn_context && after_close < b.len() && b[after_close] == b':' {
                let end_t = skip_ts_type(b, after_close + 1);
                let after_t = skip_ws_and_comments(b, end_t);
                let valid_ret = (after_t + 1 < b.len()
                    && b[after_t] == b'='
                    && b[after_t + 1] == b'>')
                    || (after_t < b.len() && b[after_t] == b'{');
                valid_ret.then_some(end_t)
            } else {
                None
            };
            let is_arrow = after_close + 1 < b.len()
                && b[after_close] == b'='
                && b[after_close + 1] == b'>';
            let is_fn_before = matches!(prev_word(&out), "function" | "async")
                || {
                    let without_name = out.trim_end();
                    let name_start = without_name
                        .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '$'))
                        .map_or(0, |p| p + 1);
                    prev_word(&without_name[..name_start]) == "function"
                };
            if is_fn_before || is_arrow || return_type_end.is_some() {
                out.push('(');
                if close > i + 1 {
                    out.push_str(&strip_param_list(&code[i + 1..close - 1]));
                }
                out.push(')');
                if let Some(ret_end) = return_type_end {
                    i = ret_end;
                } else {
                    i = close;
                }
                continue;
            }
        }
        if let Some((word, end)) = read_ident(b, i) {
            if matches!(word, "const" | "let" | "var")
                && end < b.len()
                && b[end].is_ascii_whitespace()
            {
                let scan = skip_ws_and_comments(b, end);
                let after_binding = if let Some((_, after_name)) = read_ident(b, scan) {
                    Some(after_name)
                } else if scan < b.len() && matches!(b[scan], b'{' | b'[') {
                    let close = if b[scan] == b'{' { b'}' } else { b']' };
                    Some(scan_balanced(b, scan, b[scan], close))
                } else {
                    None
                };
                if let Some(after_binding) = after_binding {
                    let mut colon_pos = skip_ws_inline(b, after_binding);
                    if colon_pos < b.len() && b[colon_pos] == b'!' {
                        colon_pos = skip_ws_inline(b, colon_pos + 1);
                    }
                    if colon_pos < b.len() && b[colon_pos] == b':' {
                        out.push_str(&code[i..after_binding]);
                        i = skip_ts_type(b, colon_pos + 1);
                        continue;
                    }
                }
            } else if matches!(word, "as" | "satisfies")
                && end < b.len()
                && matches!(b[end], b' ' | b'\t')
                && prev_non_ws_byte(&out).is_some_and(|pb| {
                    is_ident_cont(pb) || matches!(pb, b')' | b']' | b'}' | b'\'' | b'"' | b'`')
                })
                && !out.trim_end().ends_with('.')
            {
                let after = skip_ws_inline(b, end);
                if after < b.len()
                    && (is_ident_start(b[after]) || matches!(b[after], b'{' | b'(' | b'['))
                {
                    let trimmed_len = out.trim_end_matches([' ', '\t']).len();
                    out.truncate(trimmed_len);
                    out.push(' ');
                    i = skip_ts_type(b, after);
                    continue;
                }
            }
            out.push_str(word);
            i = end;
            continue;
        }
    let ch = code[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Fail-closed lexical check confirming a CodeMode program can only perform
/// read-only workspace inspection with zero mutations, MCP calls, store writes,
/// subagents, `finish()`, or dynamic property dispatch (`slim[...]`).
pub(crate) fn is_strictly_readonly_program(code: &str) -> bool {
    let trimmed = code.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_CODE_BYTES {
        return false;
    }
    let b = code.as_bytes();
    let mut i = 0usize;
    let mut saw_readonly_call = false;
    while i < b.len() {
        if b[i] == b'`' {
            let mut j = i + 1;
            while j < b.len() {
                if b[j] == b'\\' {
                    j = (j + 2).min(b.len());
                    continue;
                }
                if j + 1 < b.len() && b[j] == b'$' && b[j + 1] == b'{' {
                    return false;
                }
                if b[j] == b'`' {
                    j += 1;
                    break;
                }
                j += 1;
            }
            i = j;
            continue;
        }
        if let Some(next) = scan_string_or_comment(b, i) {
            i = next;
            continue;
        }
        if let Some((word, end)) = read_ident(b, i) {
            if matches!(
                word,
                "write"
                    | "patch"
                    | "patchMany"
                    | "atomic"
                    | "shell"
                    | "tools"
                    | "store"
                    | "load"
                    | "readArtifact"
                    | "subagent"
                    | "subagent_control"
                    | "finish"
                    | "eval"
                    | "Function"
                    | "globalThis"
            ) {
                return false;
            }
            if word == "slim" {
                let next = skip_ws_and_comments(b, end);
                if b.get(next) == Some(&b'[') {
                    return false;
                }
                if b.get(next) == Some(&b'.') {
                    let prop_start = skip_ws_and_comments(b, next + 1);
                    if let Some((prop, _)) = read_ident(b, prop_start) {
                        if matches!(
                            prop,
                            "read"
                                | "readMany"
                                | "list"
                                | "search"
                                | "glob"
                                | "git"
                                | "outlineMany"
                                | "grepIndex"
                                | "graph"
                                | "code_intel"
                        ) {
                            saw_readonly_call = true;
                        } else {
                            return false;
                        }
                    } else {
                        return false;
                    }
                }
            }
            i = end;
            continue;
        }
        i += 1;
    }
    saw_readonly_call
}
