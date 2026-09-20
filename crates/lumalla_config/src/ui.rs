//! Config-side bridge for `lum.ui` → `lumalla-ui` helper.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use log::{error, warn};
use lumalla_shared::CallbackRef;
use lumalla_ui::{
    ActionSpec, ChoiceOption, FieldSpec, FieldType, FieldUpdate, FieldValue, FormResult, FormSpec,
    UiEvent, UiReply, Values,
};
use mlua::{
    Error as LuaError, IntoLua, Lua, Result as LuaResult, Table as LuaTable, Value as LuaValue,
};

/// Events delivered to the config main loop from UI helper threads.
pub enum UiHostEvent {
    /// Simple-mode helper exited.
    SimpleFinished {
        /// Submit callback.
        on_submit: Option<CallbackRef>,
        /// Cancel callback.
        on_cancel: Option<CallbackRef>,
        /// Outcome.
        outcome: SimpleOutcome,
    },
    /// Interactive NDJSON event from the helper.
    Interactive {
        /// Callbacks for this session.
        callbacks: InteractiveCallbacks,
        /// Event from helper.
        event: UiEvent,
        /// Reply channel when the helper is blocked waiting (submit/action).
        reply: Option<SyncSender<UiReply>>,
        /// Helper stdin for async updates (e.g. from `on_change`).
        stdin: Arc<Mutex<ChildStdin>>,
        /// Set when submit or cancel was already handled.
        terminal_done: Arc<AtomicBool>,
    },
    /// Interactive helper process exited.
    InteractiveClosed {
        /// Cancel callback if the session ended without a handled submit/cancel.
        on_cancel: Option<CallbackRef>,
        /// Whether submit/cancel already completed.
        terminal_done: Arc<AtomicBool>,
    },
}

/// Result of a simple-mode form.
pub enum SimpleOutcome {
    /// User submitted.
    Submitted(FormResult),
    /// User cancelled / dismissed.
    Cancelled,
    /// Helper failed.
    Failed(String),
}

/// Callbacks for an interactive session (Send via CallbackRef).
#[derive(Clone)]
pub struct InteractiveCallbacks {
    /// Terminal submit.
    pub on_submit: Option<CallbackRef>,
    /// Terminal cancel.
    pub on_cancel: Option<CallbackRef>,
    /// Optional validate before accept.
    pub validate: Option<CallbackRef>,
    /// Live field change.
    pub on_change: Option<CallbackRef>,
    /// Non-submit action.
    pub on_action: Option<CallbackRef>,
}

/// Shared handle used from Lua to start UI sessions.
#[derive(Clone)]
pub struct UiHost {
    event_tx: mpsc::Sender<UiHostEvent>,
    active: Arc<AtomicBool>,
}

impl UiHost {
    /// Create a host and the receiver polled by the config event loop.
    pub fn new() -> (Self, Receiver<UiHostEvent>) {
        let (event_tx, event_rx) = mpsc::channel();
        (
            Self {
                event_tx,
                active: Arc::new(AtomicBool::new(false)),
            },
            event_rx,
        )
    }

    fn try_begin(&self) -> bool {
        self.active
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    /// Mark the session inactive (called when the helper is fully done).
    pub fn end_session(&self) {
        self.active.store(false, Ordering::SeqCst);
    }
}

/// Register `lum.ui` on the module table.
pub fn register_ui(
    lua: &Lua,
    module: &LuaTable,
    callback_state: crate::callback::CallbackState,
    ui_host: UiHost,
) -> LuaResult<()> {
    let host = ui_host;
    let callbacks = callback_state;
    module.set(
        "ui",
        lua.create_function(move |lua, spec: LuaTable| {
            start_ui(lua, spec, &callbacks, &host)
        })?,
    )?;
    Ok(())
}

fn start_ui(
    lua: &Lua,
    spec_table: LuaTable,
    callback_state: &crate::callback::CallbackState,
    host: &UiHost,
) -> LuaResult<()> {
    if !host.try_begin() {
        return Err(LuaError::runtime(
            "lum.ui: a form is already open (only one at a time)",
        ));
    }

    let parsed = match parse_ui_spec(lua, &spec_table, callback_state) {
        Ok(parsed) => parsed,
        Err(err) => {
            host.end_session();
            return Err(err);
        }
    };

    let spec_path = match write_spec_file(&parsed.spec) {
        Ok(path) => path,
        Err(err) => {
            host.end_session();
            return Err(LuaError::runtime(format!(
                "lum.ui: failed to write form spec: {err}"
            )));
        }
    };

    let binary = ui_binary();
    let mut cmd = Command::new(&binary);
    if parsed.interactive {
        cmd.arg("--interactive");
    } else {
        cmd.arg("--simple");
    }
    cmd.arg("--spec").arg(&spec_path);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::inherit());

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            host.end_session();
            let _ = fs::remove_file(&spec_path);
            return Err(LuaError::runtime(format!(
                "lum.ui: failed to spawn {}: {err}",
                binary.display()
            )));
        }
    };

    let event_tx = host.event_tx.clone();
    let active = host.active.clone();
    let interactive = parsed.interactive;
    let on_submit = parsed.on_submit;
    let on_cancel = parsed.on_cancel;
    let validate = parsed.validate;
    let on_change = parsed.on_change;
    let on_action = parsed.on_action;

    if interactive {
        let stdout = child.stdout.take();
        let stdin = child.stdin.take();
        let Some(stdout) = stdout else {
            host.end_session();
            let _ = child.kill();
            let _ = fs::remove_file(&spec_path);
            return Err(LuaError::runtime("lum.ui: missing helper stdout"));
        };
        let Some(stdin) = stdin else {
            host.end_session();
            let _ = child.kill();
            let _ = fs::remove_file(&spec_path);
            return Err(LuaError::runtime("lum.ui: missing helper stdin"));
        };
        let stdin = Arc::new(Mutex::new(stdin));
        let terminal_done = Arc::new(AtomicBool::new(false));
        let callbacks = InteractiveCallbacks {
            on_submit,
            on_cancel,
            validate,
            on_change,
            on_action,
        };
        let stdin_reader = stdin.clone();
        let event_tx2 = event_tx.clone();
        let callbacks_bridge = callbacks.clone();
        let terminal_bridge = terminal_done.clone();
        let on_cancel_closed = callbacks.on_cancel;

        thread::Builder::new()
            .name(String::from("lumalla-ui-bridge"))
            .spawn(move || {
                run_interactive_bridge(
                    stdout,
                    stdin_reader,
                    event_tx2,
                    callbacks_bridge,
                    terminal_bridge.clone(),
                );
                let _ = child.wait();
                let _ = fs::remove_file(&spec_path);
                let _ = event_tx.send(UiHostEvent::InteractiveClosed {
                    on_cancel: on_cancel_closed,
                    terminal_done: terminal_bridge,
                });
                active.store(false, Ordering::SeqCst);
            })
            .map_err(|err| {
                host.end_session();
                LuaError::runtime(format!("lum.ui: failed to spawn bridge thread: {err}"))
            })?;
    } else {
        let stdout = child.stdout.take();
        thread::Builder::new()
            .name(String::from("lumalla-ui-simple"))
            .spawn(move || {
                let outcome = wait_simple(stdout, &mut child);
                let _ = fs::remove_file(&spec_path);
                let _ = event_tx.send(UiHostEvent::SimpleFinished {
                    on_submit,
                    on_cancel,
                    outcome,
                });
                active.store(false, Ordering::SeqCst);
            })
            .map_err(|err| {
                host.end_session();
                LuaError::runtime(format!("lum.ui: failed to spawn wait thread: {err}"))
            })?;
    }

    Ok(())
}

fn wait_simple(stdout: Option<std::process::ChildStdout>, child: &mut Child) -> SimpleOutcome {
    let status = match child.wait() {
        Ok(status) => status,
        Err(err) => return SimpleOutcome::Failed(err.to_string()),
    };
    let code = status.code();
    if code == Some(0) {
        let Some(stdout) = stdout else {
            return SimpleOutcome::Failed(String::from("missing helper stdout"));
        };
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        return match reader.read_line(&mut line) {
            Ok(0) => SimpleOutcome::Failed(String::from("helper exited 0 with empty stdout")),
            Ok(_) => match serde_json::from_str::<FormResult>(line.trim()) {
                Ok(result) => SimpleOutcome::Submitted(result),
                Err(err) => SimpleOutcome::Failed(format!("invalid form result: {err}")),
            },
            Err(err) => SimpleOutcome::Failed(err.to_string()),
        };
    }
    // Exit 1 is the helper's intentional cancel / window close.
    if code == Some(1) {
        return SimpleOutcome::Cancelled;
    }
    SimpleOutcome::Failed(format!(
        "lumalla-ui exited with status {status} (is Wayland/Vulkan available? check helper stderr)"
    ))
}

fn run_interactive_bridge(
    stdout: std::process::ChildStdout,
    stdin: Arc<Mutex<ChildStdin>>,
    event_tx: mpsc::Sender<UiHostEvent>,
    callbacks: InteractiveCallbacks,
    terminal_done: Arc<AtomicBool>,
) {
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let event: UiEvent = match serde_json::from_str(trimmed) {
                    Ok(event) => event,
                    Err(err) => {
                        warn!("lum.ui: invalid UiEvent from helper: {err} ({trimmed})");
                        continue;
                    }
                };
                let needs_reply = matches!(
                    event,
                    UiEvent::Submit { .. } | UiEvent::Action { .. } | UiEvent::Validate { .. }
                );
                let reply = if needs_reply {
                    let (tx, rx) = mpsc::sync_channel(1);
                    if event_tx
                        .send(UiHostEvent::Interactive {
                            callbacks: callbacks.clone(),
                            event,
                            reply: Some(tx),
                            stdin: stdin.clone(),
                            terminal_done: terminal_done.clone(),
                        })
                        .is_err()
                    {
                        break;
                    }
                    match rx.recv_timeout(Duration::from_secs(120)) {
                        Ok(reply) => Some(reply),
                        Err(_) => {
                            warn!("lum.ui: timed out waiting for Lua reply");
                            Some(UiReply::Error {
                                message: String::from("config reply timed out"),
                            })
                        }
                    }
                } else {
                    if event_tx
                        .send(UiHostEvent::Interactive {
                            callbacks: callbacks.clone(),
                            event,
                            reply: None,
                            stdin: stdin.clone(),
                            terminal_done: terminal_done.clone(),
                        })
                        .is_err()
                    {
                        break;
                    }
                    None
                };

                if let Some(reply) = reply {
                    if let Ok(mut guard) = stdin.lock() {
                        if let Err(err) = write_reply(&mut guard, &reply) {
                            warn!("lum.ui: failed to write reply: {err}");
                            break;
                        }
                    }
                }
            }
            Err(err) => {
                warn!("lum.ui: helper stdout error: {err}");
                break;
            }
        }
    }
}

fn write_reply(stdin: &mut ChildStdin, reply: &UiReply) -> anyhow::Result<()> {
    serde_json::to_writer(&mut *stdin, reply)?;
    stdin.write_all(b"\n")?;
    stdin.flush()?;
    Ok(())
}

/// Write an async update to the helper (from the main thread).
pub fn write_ui_reply(stdin: &Arc<Mutex<ChildStdin>>, reply: &UiReply) {
    match stdin.lock() {
        Ok(mut guard) => {
            if let Err(err) = write_reply(&mut guard, reply) {
                warn!("lum.ui: failed to write async reply: {err}");
            }
        }
        Err(err) => warn!("lum.ui: stdin lock poisoned: {err}"),
    }
}

fn write_spec_file(spec: &FormSpec) -> anyhow::Result<PathBuf> {
    let path = std::env::temp_dir().join(format!(
        "lumalla-ui-spec-{}-{}.json",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let json = serde_json::to_vec_pretty(spec)?;
    fs::write(&path, json)?;
    Ok(path)
}

fn ui_binary() -> PathBuf {
    if let Ok(path) = std::env::var("LUMALLA_UI") {
        return PathBuf::from(path);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join("lumalla-ui");
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    PathBuf::from("lumalla-ui")
}

struct ParsedUiSpec {
    spec: FormSpec,
    interactive: bool,
    on_submit: Option<CallbackRef>,
    on_cancel: Option<CallbackRef>,
    validate: Option<CallbackRef>,
    on_change: Option<CallbackRef>,
    on_action: Option<CallbackRef>,
}

fn parse_ui_spec(
    _lua: &Lua,
    table: &LuaTable,
    callback_state: &crate::callback::CallbackState,
) -> LuaResult<ParsedUiSpec> {
    let title: String = table.get("title").unwrap_or_default();

    let mut fields = Vec::new();
    if let Ok(fields_table) = table.get::<LuaTable>("fields") {
        for pair in fields_table.sequence_values::<LuaTable>() {
            fields.push(parse_field(pair?)?);
        }
    }

    let mut actions = Vec::new();
    if let Ok(actions_table) = table.get::<LuaTable>("actions") {
        for pair in actions_table.sequence_values::<LuaTable>() {
            actions.push(parse_action(pair?)?);
        }
    }

    let on_submit = take_callback(table, "on_submit", callback_state)?;
    let on_cancel = take_callback(table, "on_cancel", callback_state)?;
    let validate = take_callback(table, "validate", callback_state)?;
    let on_change = take_callback(table, "on_change", callback_state)?;
    let on_action = take_callback(table, "on_action", callback_state)?;

    let interactive = validate.is_some() || on_change.is_some() || on_action.is_some();

    Ok(ParsedUiSpec {
        spec: FormSpec {
            title,
            fields,
            actions,
        },
        interactive,
        on_submit,
        on_cancel,
        validate,
        on_change,
        on_action,
    })
}

fn take_callback(
    table: &LuaTable,
    key: &str,
    callback_state: &crate::callback::CallbackState,
) -> LuaResult<Option<CallbackRef>> {
    match table.get::<LuaValue>(key)? {
        LuaValue::Nil => Ok(None),
        LuaValue::Function(func) => Ok(Some(callback_state.register_callback(func))),
        other => Err(LuaError::runtime(format!(
            "lum.ui: `{key}` must be a function, got {other:?}"
        ))),
    }
}

fn parse_field(table: LuaTable) -> LuaResult<FieldSpec> {
    let id: String = table.get("id")?;
    let type_str: String = table.get("type")?;
    let field_type = match type_str.as_str() {
        "text" => FieldType::Text,
        "choice" => FieldType::Choice,
        "toggle" => FieldType::Toggle,
        "label" => FieldType::Label,
        other => {
            return Err(LuaError::runtime(format!(
                "lum.ui: unknown field type `{other}`"
            )));
        }
    };
    let label: String = table.get("label").unwrap_or_default();
    let placeholder: Option<String> = table.get("placeholder").ok();
    let password: bool = table.get("password").unwrap_or(false);
    let focus: bool = table.get("focus").unwrap_or(false);
    let default = match table.get::<LuaValue>("default")? {
        LuaValue::Nil => None,
        value => Some(lua_to_field_value(value)?),
    };
    let mut options = Vec::new();
    if let Ok(opts) = table.get::<LuaTable>("options") {
        for pair in opts.sequence_values::<LuaValue>() {
            options.push(parse_choice_option(pair?)?);
        }
    }
    Ok(FieldSpec {
        id,
        field_type,
        label,
        placeholder,
        default,
        password,
        focus,
        options,
        error: None,
    })
}

fn parse_choice_option(value: LuaValue) -> LuaResult<ChoiceOption> {
    match value {
        LuaValue::String(s) => Ok(ChoiceOption::Id(s.to_str()?.to_owned())),
        LuaValue::Table(t) => {
            let id: String = t.get("id")?;
            let label: String = t.get("label").unwrap_or_default();
            Ok(ChoiceOption::Labeled { id, label })
        }
        other => Err(LuaError::runtime(format!(
            "lum.ui: choice option must be string or table, got {other:?}"
        ))),
    }
}

fn parse_action(table: LuaTable) -> LuaResult<ActionSpec> {
    let id: String = table.get("id")?;
    let label: String = table.get("label").unwrap_or_default();
    let primary: bool = table.get("primary").unwrap_or(false);
    let submit: bool = table.get("submit").unwrap_or(false);
    Ok(ActionSpec {
        id,
        label,
        primary,
        submit,
    })
}

fn lua_to_field_value(value: LuaValue) -> LuaResult<FieldValue> {
    match value {
        LuaValue::String(s) => Ok(FieldValue::String(s.to_str()?.to_owned())),
        LuaValue::Boolean(b) => Ok(FieldValue::Bool(b)),
        LuaValue::Integer(i) => Ok(FieldValue::String(i.to_string())),
        LuaValue::Number(n) => Ok(FieldValue::String(n.to_string())),
        other => Err(LuaError::runtime(format!(
            "lum.ui: unsupported field value {other:?}"
        ))),
    }
}

/// Convert form values to a Lua table.
pub fn values_to_lua(lua: &Lua, values: &Values) -> LuaResult<LuaTable> {
    let table = lua.create_table()?;
    for (key, value) in values {
        match value {
            FieldValue::String(s) => table.set(key.as_str(), s.as_str())?,
            FieldValue::Bool(b) => table.set(key.as_str(), *b)?,
        }
    }
    Ok(table)
}

/// Convert a field value to Lua.
pub fn field_value_to_lua(lua: &Lua, value: &FieldValue) -> LuaResult<LuaValue> {
    match value {
        FieldValue::String(s) => s.as_str().into_lua(lua),
        FieldValue::Bool(b) => b.into_lua(lua),
    }
}

/// Interpret a Lua validate / on_action return as a reply.
pub fn lua_result_to_reply(lua: &Lua, multi: mlua::MultiValue) -> LuaResult<UiReply> {
    let mut vals: Vec<LuaValue> = multi.into_iter().collect();
    if vals.is_empty() {
        return Ok(UiReply::Ok);
    }
    match vals.remove(0) {
        LuaValue::Nil | LuaValue::Boolean(true) => Ok(UiReply::Ok),
        LuaValue::Boolean(false) => {
            let message = match vals.first() {
                Some(LuaValue::String(s)) => s.to_str()?.to_owned(),
                _ => String::from("Invalid"),
            };
            Ok(UiReply::Error { message })
        }
        LuaValue::String(s) => Ok(UiReply::Error {
            message: s.to_str()?.to_owned(),
        }),
        LuaValue::Table(t) => {
            // Treat as update table: { fields = { ... } } or a list of field updates.
            let fields = parse_field_updates(lua, &t)?;
            Ok(UiReply::Update { fields })
        }
        other => Err(LuaError::runtime(format!(
            "lum.ui: unexpected callback return {other:?}"
        ))),
    }
}

fn parse_field_updates(_lua: &Lua, table: &LuaTable) -> LuaResult<Vec<FieldUpdate>> {
    if let Ok(fields) = table.get::<LuaTable>("fields") {
        let mut out = Vec::new();
        for pair in fields.sequence_values::<LuaTable>() {
            out.push(parse_field_update(pair?)?);
        }
        return Ok(out);
    }
    // Bare sequence of field updates.
    let mut out = Vec::new();
    let mut is_sequence = false;
    for pair in table.sequence_values::<LuaTable>() {
        is_sequence = true;
        out.push(parse_field_update(pair?)?);
    }
    if is_sequence {
        return Ok(out);
    }
    // Single field update table with `id`.
    if table.contains_key("id")? {
        return Ok(vec![parse_field_update(table.clone())?]);
    }
    Ok(Vec::new())
}

fn parse_field_update(table: LuaTable) -> LuaResult<FieldUpdate> {
    let id: String = table.get("id")?;
    let error: Option<String> = match table.get::<LuaValue>("error")? {
        LuaValue::Nil => None,
        LuaValue::String(s) => Some(s.to_str()?.to_owned()),
        other => {
            return Err(LuaError::runtime(format!(
                "lum.ui: field update error must be string, got {other:?}"
            )));
        }
    };
    let value = match table.get::<LuaValue>("value")? {
        LuaValue::Nil => None,
        v => Some(lua_to_field_value(v)?),
    };
    let label: Option<String> = table.get("label").ok();
    let placeholder: Option<String> = table.get("placeholder").ok();
    let options = if let Ok(opts) = table.get::<LuaTable>("options") {
        let mut list = Vec::new();
        for pair in opts.sequence_values::<LuaValue>() {
            list.push(parse_choice_option(pair?)?);
        }
        Some(list)
    } else {
        None
    };
    Ok(FieldUpdate {
        id,
        error,
        value,
        label,
        placeholder,
        options,
    })
}

/// Dispatch a simple-mode finish on the Lua side.
pub fn handle_simple_finished(
    callback_state: &crate::callback::CallbackState,
    lua: &Lua,
    on_submit: Option<CallbackRef>,
    on_cancel: Option<CallbackRef>,
    outcome: SimpleOutcome,
) {
    match outcome {
        SimpleOutcome::Submitted(result) => {
            if let Some(cb) = on_submit {
                match values_to_lua(lua, &result.values) {
                    Ok(values) => {
                        if let Err(err) = callback_state
                            .run_callback::<(LuaTable, String), ()>(cb, (values, result.action))
                        {
                            warn!("lum.ui on_submit failed: {err:#}");
                        }
                    }
                    Err(err) => warn!("lum.ui: failed to convert values: {err}"),
                }
            }
        }
        SimpleOutcome::Cancelled => {
            if let Some(cb) = on_cancel {
                if let Err(err) = callback_state.run_callback::<(), ()>(cb, ()) {
                    warn!("lum.ui on_cancel failed: {err:#}");
                }
            }
        }
        SimpleOutcome::Failed(msg) => {
            error!("lum.ui helper failed: {msg}");
            if let Some(cb) = on_cancel {
                if let Err(err) = callback_state.run_callback::<(), ()>(cb, ()) {
                    warn!("lum.ui on_cancel failed: {err:#}");
                }
            }
        }
    }
}

/// Handle one interactive event on the Lua side; returns the reply to send (if any).
pub fn handle_interactive_event(
    callback_state: &crate::callback::CallbackState,
    lua: &Lua,
    callbacks: &InteractiveCallbacks,
    event: UiEvent,
    stdin: &Arc<Mutex<ChildStdin>>,
    terminal_done: &AtomicBool,
) -> Option<UiReply> {
    match event {
        UiEvent::Change { id, value, values } => {
            if let Some(cb) = callbacks.on_change {
                match (|| -> LuaResult<mlua::MultiValue> {
                    let values_tbl = values_to_lua(lua, &values)?;
                    let value_lua = field_value_to_lua(lua, &value)?;
                    callback_state
                        .run_callback::<(String, LuaValue, LuaTable), mlua::MultiValue>(
                            cb,
                            (id, value_lua, values_tbl),
                        )
                        .map_err(|err| LuaError::runtime(err.to_string()))
                })() {
                    Ok(multi) => match lua_result_to_reply(lua, multi) {
                        Ok(UiReply::Ok) => {}
                        Ok(reply) => write_ui_reply(stdin, &reply),
                        Err(err) => warn!("lum.ui on_change return error: {err}"),
                    },
                    Err(err) => warn!("lum.ui on_change failed: {err}"),
                }
            }
            None
        }
        UiEvent::Validate { values } => Some(run_validate(callback_state, lua, callbacks, &values)),
        UiEvent::Action { action, values } => {
            Some(run_on_action(callback_state, lua, callbacks, &action, &values))
        }
        UiEvent::Submit { action, values } => {
            let validate_reply = run_validate(callback_state, lua, callbacks, &values);
            if !matches!(validate_reply, UiReply::Ok) {
                return Some(validate_reply);
            }
            if let Some(cb) = callbacks.on_submit {
                match values_to_lua(lua, &values) {
                    Ok(values_tbl) => {
                        if let Err(err) = callback_state
                            .run_callback::<(LuaTable, String), ()>(cb, (values_tbl, action))
                        {
                            warn!("lum.ui on_submit failed: {err:#}");
                            return Some(UiReply::Error {
                                message: format!("on_submit failed: {err}"),
                            });
                        }
                    }
                    Err(err) => {
                        return Some(UiReply::Error {
                            message: format!("values conversion failed: {err}"),
                        });
                    }
                }
            }
            terminal_done.store(true, Ordering::SeqCst);
            Some(UiReply::Ok)
        }
        UiEvent::Cancel => {
            if !terminal_done.swap(true, Ordering::SeqCst) {
                if let Some(cb) = callbacks.on_cancel {
                    if let Err(err) = callback_state.run_callback::<(), ()>(cb, ()) {
                        warn!("lum.ui on_cancel failed: {err:#}");
                    }
                }
            }
            None
        }
    }
}

fn run_validate(
    callback_state: &crate::callback::CallbackState,
    lua: &Lua,
    callbacks: &InteractiveCallbacks,
    values: &Values,
) -> UiReply {
    let Some(cb) = callbacks.validate else {
        return UiReply::Ok;
    };
    match values_to_lua(lua, values) {
        Ok(values_tbl) => match callback_state.run_callback::<LuaTable, mlua::MultiValue>(cb, values_tbl)
        {
            Ok(multi) => match lua_result_to_reply(lua, multi) {
                Ok(reply) => reply,
                Err(err) => UiReply::Error {
                    message: err.to_string(),
                },
            },
            Err(err) => UiReply::Error {
                message: format!("validate failed: {err}"),
            },
        },
        Err(err) => UiReply::Error {
            message: err.to_string(),
        },
    }
}

fn run_on_action(
    callback_state: &crate::callback::CallbackState,
    lua: &Lua,
    callbacks: &InteractiveCallbacks,
    action: &str,
    values: &Values,
) -> UiReply {
    let Some(cb) = callbacks.on_action else {
        return UiReply::Ok;
    };
    match values_to_lua(lua, values) {
        Ok(values_tbl) => {
            match callback_state
                .run_callback::<(String, LuaTable), mlua::MultiValue>(cb, (action.to_owned(), values_tbl))
            {
                Ok(multi) => match lua_result_to_reply(lua, multi) {
                    Ok(reply) => reply,
                    Err(err) => UiReply::Error {
                        message: err.to_string(),
                    },
                },
                Err(err) => UiReply::Error {
                    message: format!("on_action failed: {err}"),
                },
            }
        }
        Err(err) => UiReply::Error {
            message: err.to_string(),
        },
    }
}
