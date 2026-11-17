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
use std::rc::Rc;
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::time::{Duration, Instant};

use rquickjs::{CaughtError, Context, Ctx, Exception, Function, Promise};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::mpsc::{channel, Receiver};

use crate::runtime::CancellationToken;

pub(super) const MAX_JSON_BYTES: usize = 1024 * 1024;
pub(super) const MAX_CODE_BYTES: usize = 64 * 1024;
/// Console output kept per cell; the rest is counted, not stored.
pub(super) const MAX_CONSOLE_BYTES: usize = 16 * 1024;
/// How often a waiting worker rechecks cancellation and the deadline.
const POLL: Duration = Duration::from_millis(25);
const INTERRUPTED: &str = "cell cancelled or deadline exceeded";
const HOST_DISCONNECTED: &str = "CodeMode host disconnected";
const DEADLOCK: &str =
    "deadlock: the program awaits a promise that can never settle (no tool call is in flight)";

/// A synchronous helper request.
#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Command {
    Store { key: String, value: Value },
    Load { key: String },
    SearchTools(SearchArgs),
    DescribeTool { name: String },
    ListServers,
}

/// `{op: 'call', name, arguments}` from `tools.call`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CallMessage {
    #[allow(dead_code)]
    op: CallOp,
    name: String,
    arguments: Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum CallOp {
    Call,
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
    },
    /// A synchronous helper; the worker blocks on `reply`.
    Helper {
        command: Command,
        reply: SyncSender<Result<Value, String>>,
    },
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

/// What a cell produced: its result and the captured console.
pub(super) struct Output {
    pub result: Result<String, String>,
    console: Console,
}

impl Output {
    pub fn failed(error: String) -> Self {
        Self {
            result: Err(error),
            console: Console::default(),
        }
    }

    /// The result followed by `note` and the console, when there are any.
    pub fn finish(self, note: Option<&str>) -> Result<String, String> {
        let attach = |mut text: String| {
            if let Some(note) = note {
                text.push('\n');
                text.push_str(note);
            }
            self.console.attach(text)
        };
        match self.result {
            Ok(output) => Ok(attach(output)),
            Err(error) => Err(attach(error)),
        }
    }
}

/// Outcome of one `tools.call`, settling the promise of `ticket`.
pub(super) struct Completion {
    pub ticket: u32,
    pub result: Result<Value, String>,
}

pub(super) struct Cell {
    pub requests: Receiver<Request>,
    pub worker: tokio::task::JoinHandle<Output>,
    /// Cancelled by the run, by the deadline, by the host or on drop. Native
    /// calls observe it directly and MCP calls through its MCP handle.
    pub cancellation: CancellationToken,
    pub deadline: Instant,
    completions: mpsc::Sender<Completion>,
}

impl Cell {
    /// Settles the promise of `ticket`. A worker that already stopped no
    /// longer listens, which is not an error.
    pub fn settle(&self, ticket: u32, result: Result<Value, String>) {
        let _ = self.completions.send(Completion { ticket, result });
    }
}

impl Drop for Cell {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

/// Defines the globals of a cell and returns the function that settles a
/// ticket. `natives` names the `slim.<name>` functions (Code mode only).
const PRELUDE: &str = r#"
((host, log, natives, stringify, parse) => {
    const request = value => parse(host(stringify(value)));
    const pending = new Map();
    const call = (name, arguments_ = {}) => new Promise((resolve, reject) => {
        pending.set(request({op: 'call', name, arguments: arguments_}), {resolve, reject});
    });
    globalThis.tools = Object.freeze({call});
    if (natives.length > 0) {
        globalThis.slim = Object.freeze(Object.fromEntries(
            natives.map(name => [name, async (args = {}) => call('slim.' + name, args)])));
    }
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
        return request({op: 'search_tools', query, limit, server});
    };
    globalThis.describeTool = name => {
        if (typeof name !== 'string') throw new TypeError('describeTool() expects a tool name');
        return request({op: 'describe_tool', name});
    };
    globalThis.listServers = () => request({op: 'list_servers'});
    globalThis.store = (key, value) => request({op: 'store', key, value});
    globalThis.load = key => request({op: 'load', key});
    delete globalThis.__host;
    delete globalThis.__log;
    delete globalThis.__natives;
    return (ticket, ok, payload) => {
        const entry = pending.get(ticket);
        if (entry === undefined) return;
        pending.delete(ticket);
        if (!ok) return entry.reject(new Error(payload));
        const result = parse(payload);
        if (result?.isError) return entry.reject(new Error(stringify(result)));
        entry.resolve(result?.structuredContent ?? result);
    };
})(__host, __log, __natives, JSON.stringify, JSON.parse);
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

/// Tickets issued and calls still awaiting their completion.
#[derive(Default)]
struct Bridge {
    next_ticket: Counter<u32>,
    in_flight: Counter<usize>,
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
        let running = self
            .since
            .get()
            .map_or(Duration::ZERO, |since| since.elapsed());
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
    let (sender, requests) = channel::<Request>(16);
    let (completions, completed) = mpsc::channel::<Completion>();
    let cancellation = CancellationToken::new();
    let deadline = Instant::now() + limits.wall;
    watch(run.clone(), cancellation.clone(), deadline);
    let stop = cancellation.clone();
    let work = run.track_native_work();
    let worker = tokio::task::spawn_blocking(move || {
        let _work = work;
        let console = Rc::new(RefCell::new(Console::default()));
        let result = interpret(
            &code, &run, &stop, deadline, limits.cpu, sender, &natives, &console, &completed,
        );
        let console = console.take();
        Output { result, console }
    });
    Cell {
        requests,
        worker,
        cancellation,
        deadline,
        completions,
    }
}

/// Runs one program to its end, with every call it started settled.
#[allow(clippy::too_many_arguments)]
fn interpret(
    code: &str,
    run: &CancellationToken,
    stop: &CancellationToken,
    deadline: Instant,
    cpu: Duration,
    sender: tokio::sync::mpsc::Sender<Request>,
    natives: &[String],
    console: &Rc<RefCell<Console>>,
    completed: &mpsc::Receiver<Completion>,
) -> Result<String, String> {
    if code.len() > MAX_CODE_BYTES {
        return Err("code exceeds 64 KiB".into());
    }
    let runtime = rquickjs::Runtime::new().map_err(|error| error.to_string())?;
    runtime.set_memory_limit(32 * 1024 * 1024);
    runtime.set_max_stack_size(256 * 1024);
    let clock = Rc::new(CpuClock::new(cpu));
    let interrupted = {
        let (run, stop, clock) = (run.clone(), stop.clone(), Rc::clone(&clock));
        move || {
            run.is_cancelled()
                || stop.is_cancelled()
                || Instant::now() >= deadline
                || clock.exceeded()
        }
    };
    runtime.set_interrupt_handler(Some(Box::new(interrupted.clone())));
    let context = Context::full(&runtime).map_err(|error| error.to_string())?;
    let result = context.with(|ctx| {
        let caught = |error: rquickjs::Error| CaughtError::from_error(&ctx, error).to_string();
        let bridge = Rc::new(Bridge::default());
        let loop_ = EventLoop {
            ctx: &ctx,
            bridge: &bridge,
            clock: &clock,
            completions: completed,
            interrupted: &interrupted,
        };
        let returned = sender.clone();
        let settle = (|| -> rquickjs::Result<Function> {
            ctx.globals().set(
                "__host",
                host_function(
                    &ctx,
                    sender,
                    Rc::clone(&bridge),
                    Rc::clone(&clock),
                    interrupted.clone(),
                )?,
            )?;
            let sink = Rc::clone(console);
            ctx.globals().set(
                "__log",
                Function::new(ctx.clone(), move |line: String| {
                    sink.borrow_mut().push(&line)
                })?,
            )?;
            let names = serde_json::to_string(natives).unwrap_or_else(|_| "[]".into());
            ctx.globals().set("__natives", ctx.json_parse(names)?)?;
            ctx.eval(PRELUDE)
        })()
        .map_err(caught)?;
        let settled = ctx
            .eval::<Promise, _>(format!("(async () => {{\"use strict\";\n{code}\n}})()"))
            .and_then(|promise| loop_.settle_promise(&promise, &settle))
            .and_then(|value| result_json(&ctx, value))
            .map_err(caught);
        // The program ended; the calls it did not await still run to the end.
        if loop_.bridge.in_flight.get() > 0 && !interrupted() {
            let _ = returned.blocking_send(Request::Returned);
            loop_.drain(&settle).map_err(caught)?;
        }
        settled
    });
    if clock.exceeded() {
        return Err(format!(
            "JavaScript CPU time limit of {} s exceeded (waits for tool calls do not count)",
            cpu.as_secs()
        ));
    }
    result
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

/// Cancels the cell when the run is cancelled or the deadline passes, so
/// calls in flight observe one token. Ends with the cell.
fn watch(run: CancellationToken, cell: CancellationToken, deadline: Instant) {
    tokio::spawn(async move {
        tokio::select! {
            () = cell.cancelled() => {}
            () = run.cancelled() => cell.cancel(),
            () = tokio::time::sleep_until(deadline.into()) => cell.cancel(),
        }
    });
}

/// `__host(json)`: starts a call and returns its ticket, or runs a helper and
/// returns its JSON result. Failures throw into the program.
fn host_function<'js>(
    ctx: &Ctx<'js>,
    sender: tokio::sync::mpsc::Sender<Request>,
    bridge: Rc<Bridge>,
    clock: Rc<CpuClock>,
    interrupted: impl Fn() -> bool + 'static,
) -> rquickjs::Result<Function<'js>> {
    Function::new(ctx.clone(), move |ctx: Ctx<'_>, input: String| {
        let dispatch = || -> Result<String, String> {
            if input.len() > MAX_JSON_BYTES {
                return Err("host request exceeds 1 MiB".into());
            }
            if interrupted() {
                return Err(INTERRUPTED.into());
            }
            let disconnected = |_| HOST_DISCONNECTED.to_owned();
            let message: Value = serde_json::from_str(&input).map_err(|error| error.to_string())?;
            if message.get("op").and_then(Value::as_str) == Some("call") {
                let CallMessage {
                    name, arguments, ..
                } = serde_json::from_value(message).map_err(|error| error.to_string())?;
                let ticket = bridge.next_ticket.get();
                bridge.next_ticket.set(ticket.wrapping_add(1));
                clock
                    .paused(|| {
                        sender.blocking_send(Request::Call {
                            ticket,
                            name,
                            arguments,
                        })
                    })
                    .map_err(disconnected)?;
                bridge.in_flight.set(bridge.in_flight.get() + 1);
                return Ok(ticket.to_string());
            }
            let command: Command =
                serde_json::from_value(message).map_err(|error| error.to_string())?;
            let (reply, response) = mpsc::sync_channel(1);
            clock.paused(|| {
                sender
                    .blocking_send(Request::Helper { command, reply })
                    .map_err(disconnected)?;
                loop {
                    if interrupted() {
                        return Err(INTERRUPTED.into());
                    }
                    match response.recv_timeout(POLL) {
                        Ok(result) => return result.map(|value| value.to_string()),
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => return Err(HOST_DISCONNECTED.into()),
                    }
                }
            })
        };
        dispatch().map_err(|error| Exception::throw_message(&ctx, &error))
    })
}

/// The worker's event loop over the program's promises.
struct EventLoop<'a, 'js> {
    ctx: &'a Ctx<'js>,
    bridge: &'a Bridge,
    clock: &'a CpuClock,
    completions: &'a mpsc::Receiver<Completion>,
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
            self.next_completions(settle)?;
            while self.ctx.execute_pending_job() {}
        }
        Ok(())
    }

    /// Waits for the next completion and settles every one available.
    fn next_completions(&self, settle: &Function<'js>) -> rquickjs::Result<()> {
        let completion = self.clock.paused(|| loop {
            if (self.interrupted)() {
                return Err(INTERRUPTED);
            }
            match self.completions.recv_timeout(POLL) {
                Ok(completion) => return Ok(completion),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Err(HOST_DISCONNECTED),
            }
        });
        let completion = completion.map_err(|error| Exception::throw_message(self.ctx, error))?;
        settle_completion(settle, self.bridge, completion)?;
        while let Ok(completion) = self.completions.try_recv() {
            settle_completion(settle, self.bridge, completion)?;
        }
        Ok(())
    }
}

fn settle_completion<'js>(
    settle: &Function<'js>,
    bridge: &Bridge,
    completion: Completion,
) -> rquickjs::Result<()> {
    bridge
        .in_flight
        .set(bridge.in_flight.get().saturating_sub(1));
    let (ok, payload) = match completion.result {
        Ok(value) => (true, value.to_string()),
        Err(error) => (false, error),
    };
    settle.call::<_, ()>((completion.ticket, ok, payload))
}
