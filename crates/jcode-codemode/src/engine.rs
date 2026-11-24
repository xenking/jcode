//! QuickJS engine behind [`crate::run`]. Not built on FreeBSD, where
//! rquickjs-sys has no prebuilt bindings.

use super::{CallReceipt, CallStatus, Host, Limits, RunOutput, ToolInfo};
use crate::strip::Stripped;
use rquickjs::{
    AsyncContext, AsyncRuntime, CatchResultExt, Function, Promise, Value as JsValue, prelude::Async,
};
use serde_json::{Map, Value, json};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Lines the engine prepends to the script when it wraps the body in an async
/// function, so a position in the evaluated source is that many lines ahead of
/// the script the model wrote.
const WRAPPER_LINES: u32 = 1;

const PRELUDE: &str = r#"
(() => {
  const fmt = (v) => {
    if (typeof v === "string") return v;
    if (v instanceof Error) return v.stack ? `${v.name}: ${v.message}\n${v.stack}` : `${v.name}: ${v.message}`;
    try { const s = JSON.stringify(v); return s === undefined ? String(v) : s; } catch (_) { return String(v); }
  };
  const log = (level) => (...args) => __log(level, args.map(fmt).join(" "));
  globalThis.console = { log: log("log"), info: log("info"), warn: log("warn"), error: log("error"), debug: log("debug") };

  const call = async (name, args) => {
    const raw = await __callTool(String(name), JSON.stringify(args === undefined ? {} : args));
    const r = JSON.parse(raw);
    if (!r.ok) throw new Error(`${name}: ${r.error}`);
    return r.value;
  };
  globalThis.callTool = call;
  globalThis.tools = new Proxy({}, {
    get(_, name) {
      if (typeof name !== "string" || name === "then") return undefined;
      return (args) => call(name, args);
    },
    has(_, name) { return typeof name === "string" && __hasTool(name); },
    ownKeys() { return JSON.parse(__listTools()).map((t) => t.name); },
    getOwnPropertyDescriptor(_, name) {
      return __hasTool(String(name)) ? { enumerable: true, configurable: true } : undefined;
    },
  });
  globalThis.listTools = (filter) => {
    const all = JSON.parse(__listTools());
    if (!filter) return all;
    const f = String(filter).toLowerCase();
    return all.filter((t) => t.name.toLowerCase().includes(f) || t.description.toLowerCase().includes(f));
  };
  globalThis.types = (filter) => __types(filter === undefined || filter === null ? "" : String(filter));
  globalThis.describeTool = (name) => {
    const raw = __describeTool(String(name));
    return raw === "" ? undefined : JSON.parse(raw);
  };
  globalThis.store = (key, value) => { __storeSet(String(key), JSON.stringify(value === undefined ? null : value)); return value; };
  globalThis.load = (key) => { const raw = __storeGet(String(key)); return raw === "" ? undefined : JSON.parse(raw); };
  globalThis.storedKeys = () => JSON.parse(__storeKeys());
})();
"#;

/// Wall-clock limit of one run, extended by the time the host reports as
/// spent waiting on a person. Cloning shares the clock.
#[derive(Clone)]
struct Deadline {
    started: Instant,
    timeout: Duration,
    clock: Option<Arc<crate::PauseClock>>,
}

impl Deadline {
    fn at(&self) -> Instant {
        let paused = self.clock.as_ref().map_or(Duration::ZERO, |c| c.paused());
        self.started + self.timeout + paused
    }

    fn expired(&self) -> bool {
        Instant::now() >= self.at()
    }

    /// Resolves once the deadline passes. The deadline can move later while
    /// this waits, so it re-reads it after every sleep.
    async fn elapsed(&self) {
        loop {
            let at = self.at();
            if Instant::now() >= at {
                return;
            }
            tokio::time::sleep_until(at.into()).await;
        }
    }
}

struct Shared {
    host: Arc<dyn Host>,
    outer: tokio::runtime::Handle,
    tools: Vec<ToolInfo>,
    store: Mutex<Map<String, Value>>,
    logs: Mutex<(Vec<String>, usize, bool)>,
    calls: AtomicUsize,
    receipts: Mutex<Vec<CallReceipt>>,
    cancelled: Arc<AtomicBool>,
    limits: Limits,
}

impl Shared {
    fn record(
        &self,
        tool: &str,
        status: CallStatus,
        elapsed_ms: u64,
        bytes: usize,
        note: Option<String>,
    ) -> usize {
        let mut receipts = self.receipts.lock().unwrap_or_else(|e| e.into_inner());
        let seq = receipts.len() + 1;
        receipts.push(CallReceipt {
            seq,
            tool: tool.to_string(),
            status,
            elapsed_ms,
            bytes,
            note: note.map(|n| first_line(&n)),
        });
        seq
    }

    fn finish(
        &self,
        seq: usize,
        status: CallStatus,
        elapsed_ms: u64,
        bytes: usize,
        note: Option<String>,
    ) {
        let mut receipts = self.receipts.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(r) = receipts.iter_mut().find(|r| r.seq == seq) {
            r.status = status;
            r.elapsed_ms = elapsed_ms;
            r.bytes = bytes;
            r.note = note.map(|n| first_line(&n));
        }
    }

    fn push_log(&self, level: &str, line: String) {
        let mut guard = self.logs.lock().unwrap_or_else(|e| e.into_inner());
        let (lines, bytes, truncated) = &mut *guard;
        if *truncated {
            return;
        }
        let line = if level == "log" || level == "info" {
            line
        } else {
            format!("[{level}] {line}")
        };
        if *bytes + line.len() > self.limits.max_log_bytes {
            *truncated = true;
            return;
        }
        *bytes += line.len() + 1;
        lines.push(line);
    }
}

struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

pub(crate) async fn run(
    code: String,
    host: Arc<dyn Host>,
    tools: Vec<ToolInfo>,
    store: Map<String, Value>,
    limits: Limits,
) -> RunOutput {
    let cancelled = Arc::new(AtomicBool::new(false));
    let _guard = CancelOnDrop(cancelled.clone());
    let shared = Arc::new(Shared {
        host,
        outer: tokio::runtime::Handle::current(),
        tools,
        store: Mutex::new(store),
        logs: Mutex::new((Vec::new(), 0, false)),
        calls: AtomicUsize::new(0),
        receipts: Mutex::new(Vec::new()),
        cancelled,
        limits,
    });

    let (tx, rx) = tokio::sync::oneshot::channel();
    let thread_shared = shared.clone();
    let spawned = std::thread::Builder::new()
        .name("jcode-codemode".into())
        .spawn(move || {
            let started = Instant::now();
            let outcome = match tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
            {
                Ok(rt) => rt.block_on(run_on_thread(code, thread_shared.clone(), started)),
                Err(err) => Err(format!("failed to start codemode runtime: {err}")),
            };
            let _ = tx.send((outcome, started.elapsed()));
        });

    let (outcome, elapsed) = match spawned {
        Ok(_) => rx
            .await
            .unwrap_or_else(|_| (Err("codemode thread panicked".into()), Duration::ZERO)),
        Err(err) => (
            Err(format!("failed to spawn codemode thread: {err}")),
            Duration::ZERO,
        ),
    };

    let (logs, _, logs_truncated) =
        std::mem::take(&mut *shared.logs.lock().unwrap_or_else(|e| e.into_inner()));
    let store = std::mem::take(&mut *shared.store.lock().unwrap_or_else(|e| e.into_inner()));
    let (result, error) = match outcome {
        Ok(result) => (result, None),
        Err(err) => (None, Some(err)),
    };
    RunOutput {
        result,
        error,
        logs,
        logs_truncated,
        tool_calls: shared.calls.load(Ordering::SeqCst),
        receipts: std::mem::take(&mut *shared.receipts.lock().unwrap_or_else(|e| e.into_inner())),
        store,
        elapsed,
    }
}

async fn run_on_thread(
    code: String,
    shared: Arc<Shared>,
    started: Instant,
) -> Result<Option<Value>, String> {
    // Erase TypeScript types before QuickJS ever sees the script. A script that
    // does not parse as JavaScript or TypeScript returns here, so the model
    // gets its own line and column instead of a bare `unexpected token`.
    let stripped = crate::strip::strip(&code).map_err(|err| {
        format!(
            "script did not parse: {err}\ncodemode runs JavaScript and TypeScript; types are erased, so fix the syntax at that position."
        )
    })?;
    let rt = AsyncRuntime::new().map_err(|e| format!("failed to create JS runtime: {e}"))?;
    rt.set_memory_limit(shared.limits.memory_bytes).await;
    rt.set_max_stack_size(1024 * 1024).await;
    let clock = shared.host.pause_clock();
    let deadline = Deadline {
        started,
        timeout: shared.limits.timeout,
        clock,
    };
    let interrupt_cancel = shared.cancelled.clone();
    let interrupt_deadline = deadline.clone();
    rt.set_interrupt_handler(Some(Box::new(move || {
        interrupt_cancel.load(Ordering::Relaxed) || interrupt_deadline.expired()
    })))
    .await;
    let ctx = AsyncContext::full(&rt)
        .await
        .map_err(|e| format!("failed to create JS context: {e}"))?;

    let timeout = shared.limits.timeout;
    let cancel_flag = shared.cancelled.clone();
    let cancelled = async move {
        while !cancel_flag.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };

    let script = ctx.async_with(async |ctx| -> Result<Option<Value>, String> {
        install_globals(&ctx, &shared).map_err(|e| format!("failed to install globals: {e}"))?;
        ctx.eval::<(), _>(PRELUDE)
            .catch(&ctx)
            .map_err(|e| format!("prelude failed: {e}"))?;
        let wrapped = format!("(async () => {{\n{code}\n}})()", code = stripped.code);
        let promise: Promise = ctx
            .eval(wrapped)
            .catch(&ctx)
            .map_err(|e| format_js_parse_error(&stripped, &e.to_string()))?;
        let value: JsValue = promise
            .into_future::<JsValue>()
            .await
            .catch(&ctx)
            .map_err(|e| format_js_error(&stripped, &e.to_string()))?;
        if value.is_undefined() {
            return Ok(None);
        }
        let json = ctx
            .json_stringify(value)
            .catch(&ctx)
            .map_err(|e| format!("could not serialize return value: {e}"))?;
        match json {
            Some(text) => {
                let text = text.to_string().map_err(|e| e.to_string())?;
                serde_json::from_str(&text)
                    .map(Some)
                    .map_err(|e| format!("could not decode return value: {e}"))
            }
            None => Ok(None),
        }
    });

    let timeout_message = || {
        format!(
            "codemode timed out after {timeout:?} (wall-clock limit per run); split the work into shorter new codemode runs."
        )
    };
    let result = tokio::select! {
        result = script => result,
        _ = deadline.elapsed() => Err(timeout_message()),
        _ = cancelled => Err("cancelled".to_string()),
    };
    let result = match result {
        Err(err) if err.contains("interrupted") && deadline.expired() => Err(timeout_message()),
        Err(err) if err.contains("out of memory") || err.contains("memory limit reached") => {
            Err(format!(
                "{err} (codemode heap limit: {} bytes per run); process smaller batches in a new codemode run.",
                shared.limits.memory_bytes
            ))
        }
        Err(err) if err.contains("Exception generated by quickjs: Null\"null\"") => Err(format!(
            "{err}; QuickJS could not describe the exception. This can happen at the codemode heap limit ({} bytes per run); try smaller batches in a new codemode run, or inspect thrown null values.",
            shared.limits.memory_bytes
        )),
        other => other,
    };
    // Drop pending host-call futures before the runtime goes away.
    drop(ctx);
    drop(rt);
    result
}

/// QuickJS reports positions in the stripped, wrapped source. Put them back on
/// the script the model wrote before anything else looks at the message.
fn format_js_error(stripped: &Stripped, raw: &str) -> String {
    let trimmed = stripped.remap_error(raw.trim(), WRAPPER_LINES);
    if trimmed.contains("out of memory") {
        return format!("{trimmed} (memory limit reached)");
    }
    trimmed.to_string()
}

/// [`crate::strip`] has already produced the JavaScript QuickJS evaluates, and
/// it only returns code the parser and codegen agree on. Reaching a parse error
/// at this point means the stripper, not the model's script, is at fault, so
/// say that instead of sending the model off to rewrite working code.
fn format_js_parse_error(stripped: &Stripped, raw: &str) -> String {
    let message = format_js_error(stripped, raw);
    if message.contains("memory limit reached") {
        return message;
    }
    format!(
        "{message}\nThe stripped script did not parse. That is a codemode bug rather than a problem with your script: nothing about your code was rejected for its types.\n"
    )
}

fn install_globals<'js>(ctx: &rquickjs::Ctx<'js>, shared: &Arc<Shared>) -> rquickjs::Result<()> {
    let globals = ctx.globals();

    let s = shared.clone();
    globals.set(
        "__log",
        Function::new(ctx.clone(), move |level: String, line: String| {
            s.push_log(&level, line);
        })?,
    )?;

    let s = shared.clone();
    globals.set(
        "__listTools",
        Function::new(ctx.clone(), move || -> String {
            let list: Vec<Value> = s
                .tools
                .iter()
                .map(|t| json!({ "name": t.name, "description": first_line(&t.description) }))
                .collect();
            Value::Array(list).to_string()
        })?,
    )?;

    let s = shared.clone();
    globals.set(
        "__hasTool",
        Function::new(ctx.clone(), move |name: String| -> bool {
            s.tools.iter().any(|t| t.name == name)
        })?,
    )?;

    let s = shared.clone();
    globals.set(
        "__describeTool",
        Function::new(ctx.clone(), move |name: String| -> String {
            s.tools
                .iter()
                .find(|t| t.name == name)
                .map(|t| {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.input_schema,
                        "output_schema": t.output_schema,
                        "typescript": crate::ts::declarations(std::iter::once(t)),
                    })
                    .to_string()
                })
                .unwrap_or_default()
        })?,
    )?;

    let s = shared.clone();
    globals.set(
        "__types",
        Function::new(ctx.clone(), move |filter: String| -> String {
            let f = filter.to_lowercase();
            crate::ts::declarations(s.tools.iter().filter(|t| {
                f.is_empty()
                    || t.name.to_lowercase().contains(&f)
                    || t.description.to_lowercase().contains(&f)
            }))
        })?,
    )?;

    let s = shared.clone();
    globals.set(
        "__storeSet",
        Function::new(ctx.clone(), move |key: String, raw: String| {
            let value = serde_json::from_str(&raw).unwrap_or(Value::Null);
            s.store
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key, value);
        })?,
    )?;

    let s = shared.clone();
    globals.set(
        "__storeGet",
        Function::new(ctx.clone(), move |key: String| -> String {
            s.store
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&key)
                .map(Value::to_string)
                .unwrap_or_default()
        })?,
    )?;

    let s = shared.clone();
    globals.set(
        "__storeKeys",
        Function::new(ctx.clone(), move || -> String {
            let keys: Vec<Value> = s
                .store
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .keys()
                .cloned()
                .map(Value::String)
                .collect();
            Value::Array(keys).to_string()
        })?,
    )?;

    let s = shared.clone();
    globals.set(
        "__callTool",
        Function::new(
            ctx.clone(),
            Async(move |name: String, raw_args: String| {
                let s = s.clone();
                async move { Ok::<String, rquickjs::Error>(call_tool(&s, name, raw_args).await) }
            }),
        )?,
    )?;

    Ok(())
}

async fn call_tool(shared: &Arc<Shared>, name: String, raw_args: String) -> String {
    let envelope = |result: Result<Value, String>| match result {
        Ok(value) => json!({ "ok": true, "value": value }).to_string(),
        Err(error) => json!({ "ok": false, "error": error }).to_string(),
    };
    let refuse = |name: &str, msg: String| {
        shared.record(name, CallStatus::Refused, 0, 0, Some(msg.clone()));
        envelope(Err(msg))
    };
    if shared.cancelled.load(Ordering::Relaxed) {
        return refuse(&name, "cancelled".into());
    }
    if !shared.tools.iter().any(|t| t.name == name) {
        let mut hint = String::new();
        let lower = name.to_lowercase();
        let similar: Vec<&str> = shared
            .tools
            .iter()
            .filter(|t| {
                t.name.to_lowercase().contains(&lower) || lower.contains(&t.name.to_lowercase())
            })
            .map(|t| t.name.as_str())
            .take(5)
            .collect();
        if !similar.is_empty() {
            hint = format!(" Did you mean: {}?", similar.join(", "));
        }
        let msg = format!("unknown tool '{name}'.{hint} Use listTools() to see available tools.");
        return refuse(&name, msg);
    }
    let count = shared.calls.fetch_add(1, Ordering::SeqCst) + 1;
    if count > shared.limits.max_tool_calls {
        let msg = format!(
            "tool call limit reached ({} per run); split the work into a new codemode run and reuse stored results where appropriate.",
            shared.limits.max_tool_calls
        );
        return refuse(&name, msg);
    }
    let args: Value = match serde_json::from_str(&raw_args) {
        Ok(Value::Null) => Value::Object(Map::new()),
        Ok(value) => value,
        Err(err) => {
            return refuse(
                &name,
                format!(
                    "arguments are not JSON: {err}; pass JSON-serializable arguments and retry."
                ),
            );
        }
    };
    let seq = shared.record(&name, CallStatus::InFlight, 0, 0, None);
    let started = Instant::now();
    let fut = shared.host.call_tool(name, args);
    // Abort the host call if the script is torn down (timeout or cancel)
    // while this call is still in flight.
    struct AbortOnDrop(tokio::task::JoinHandle<Result<Value, String>>);
    impl Drop for AbortOnDrop {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let mut handle = AbortOnDrop(shared.outer.spawn(fut));
    let result = match (&mut handle.0).await {
        Ok(result) => result,
        Err(err) => Err(format!("tool task failed: {err}")),
    };
    let elapsed_ms = started.elapsed().as_millis() as u64;
    match &result {
        Ok(value) => {
            let bytes = match value {
                Value::String(s) => s.len(),
                other => other.to_string().len(),
            };
            shared.finish(seq, CallStatus::Ok, elapsed_ms, bytes, None);
        }
        Err(err) => shared.finish(seq, CallStatus::Failed, elapsed_ms, 0, Some(err.clone())),
    }
    envelope(result)
}

fn first_line(text: &str) -> String {
    let line = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    if line.chars().count() > 200 {
        let cut: String = line.chars().take(200).collect();
        format!("{cut}…")
    } else {
        line.to_string()
    }
}
