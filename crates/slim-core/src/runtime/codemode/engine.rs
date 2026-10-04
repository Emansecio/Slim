//! One disposable interpreter per cell. Only JSON crosses the host boundary.
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::time::{Duration, Instant};

use rquickjs::{CaughtError, Context, Exception, Function, Promise};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::mpsc::{channel, Receiver};

use crate::mcp::McpCancellation;
use crate::runtime::CancellationToken;

pub(super) const MAX_JSON_BYTES: usize = 1024 * 1024;
pub(super) const MAX_CODE_BYTES: usize = 64 * 1024;

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Command {
    Call { name: String, arguments: Value },
    Store { key: String, value: Value },
    Load { key: String },
    SearchTools(SearchArgs),
    DescribeTool { name: String },
    ListServers,
}

/// Arguments of `searchTools(query, {limit, server})`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SearchArgs {
    pub query: String,
    pub limit: Option<usize>,
    pub server: Option<String>,
}

pub(super) struct Request {
    pub command: Command,
    pub reply: SyncSender<Result<Value, String>>,
}

pub(super) struct Cell {
    pub requests: Receiver<Request>,
    pub worker: tokio::task::JoinHandle<Result<String, String>>,
    pub cancellation: McpCancellation,
    pub deadline: Instant,
}

impl Drop for Cell {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

const PRELUDE: &str = r#"
((host, stringify, parse) => {
    const request = value => parse(host(stringify(value)));
    globalThis.tools = Object.freeze({
        call: async (name, arguments_ = {}) => {
            const result = request({op: 'call', name, arguments: arguments_});
            if (result.isError) throw new Error(stringify(result));
            return result.structuredContent ?? result;
        }
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
})(__host, JSON.stringify, JSON.parse);
"#;

pub(super) fn spawn(code: String, run: CancellationToken, timeout: Duration) -> Cell {
    let (sender, requests) = channel::<Request>(1);
    let cancellation = McpCancellation::new();
    let stop = cancellation.clone();
    let deadline = Instant::now() + timeout;
    let work = run.track_native_work();
    let worker = tokio::task::spawn_blocking(move || {
        let _work = work;
        if code.len() > MAX_CODE_BYTES {
            return Err("code exceeds 64 KiB".into());
        }
        let runtime = rquickjs::Runtime::new().map_err(|error| error.to_string())?;
        runtime.set_memory_limit(32 * 1024 * 1024);
        runtime.set_max_stack_size(256 * 1024);
        let interrupted = {
            let run = run.clone();
            let stop = stop.clone();
            move || run.is_cancelled() || stop.is_cancelled() || Instant::now() >= deadline
        };
        runtime.set_interrupt_handler(Some(Box::new(interrupted)));
        let context = Context::full(&runtime).map_err(|error| error.to_string())?;
        context.with(|ctx| {
            let execute = || -> rquickjs::Result<String> {
                let host =
                    Function::new(ctx.clone(), move |ctx: rquickjs::Ctx<'_>, input: String| {
                        let dispatch = || -> Result<String, String> {
                            if input.len() > MAX_JSON_BYTES {
                                return Err("host request exceeds 1 MiB".into());
                            }
                            if run.is_cancelled()
                                || stop.is_cancelled()
                                || Instant::now() >= deadline
                            {
                                return Err("cell cancelled or deadline exceeded".into());
                            }
                            let command =
                                serde_json::from_str(&input).map_err(|error| error.to_string())?;
                            let (reply, response) = mpsc::sync_channel(1);
                            sender
                                .blocking_send(Request { command, reply })
                                .map_err(|_| "CodeMode host disconnected".to_owned())?;
                            loop {
                                if run.is_cancelled()
                                    || stop.is_cancelled()
                                    || Instant::now() >= deadline
                                {
                                    return Err("cell cancelled or deadline exceeded".into());
                                }
                                match response.recv_timeout(Duration::from_millis(25)) {
                                    Ok(result) => return result.map(|value| value.to_string()),
                                    Err(RecvTimeoutError::Timeout) => {}
                                    Err(RecvTimeoutError::Disconnected) => {
                                        return Err("CodeMode host disconnected".into())
                                    }
                                }
                            }
                        };
                        dispatch().map_err(|error| Exception::throw_message(&ctx, &error))
                    })?;
                ctx.globals().set("__host", host)?;
                ctx.eval::<(), _>(PRELUDE)?;
                let promise: Promise =
                    ctx.eval(format!("(async () => {{\"use strict\";\n{code}\n}})()"))?;
                let value = promise.finish::<rquickjs::Value>()?;
                let output = ctx
                    .json_stringify(value)?
                    .map(|text| text.to_string())
                    .transpose()?
                    .unwrap_or_else(|| "null".into());
                if output.len() > MAX_JSON_BYTES {
                    return Err(Exception::throw_message(
                        &ctx,
                        "cell result exceeds 1 MiB; return a smaller summary",
                    ));
                }
                Ok(output)
            };
            execute().map_err(|error| CaughtError::from_error(&ctx, error).to_string())
        })
    });
    Cell {
        requests,
        worker,
        cancellation,
        deadline,
    }
}
