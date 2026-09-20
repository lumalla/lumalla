//! egui form window (wgpu / Vulkan — Lumalla has no EGL for glow).

use std::io::{BufRead, BufReader, Write};
use std::process::ExitCode;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;

use eframe::egui::{self, Color32, Key, RichText, ViewportBuilder, ViewportCommand};
use log::{debug, warn};
use lumalla_ui::{
    FieldType, FieldUpdate, FieldValue, FormResult, FormSpec, UiEvent, UiReply, Values,
};

/// Run mode for the helper process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Exit with a single JSON result.
    Simple,
    /// NDJSON protocol on stdin/stdout.
    Interactive,
}

enum PendingRequest {
    Action,
    Submit(String),
}

struct FormApp {
    spec: FormSpec,
    values: Values,
    mode: Mode,
    form_error: Option<String>,
    pending: Option<PendingRequest>,
    reply_rx: Option<Receiver<UiReply>>,
    exit_code: Option<u8>,
    result: Option<FormResult>,
    focus_applied: bool,
}

impl FormApp {
    fn new(spec: FormSpec, mode: Mode, reply_rx: Option<Receiver<UiReply>>) -> Self {
        let values = spec.initial_values();
        Self {
            spec,
            values,
            mode,
            form_error: None,
            pending: None,
            reply_rx,
            exit_code: None,
            result: None,
            focus_applied: false,
        }
    }

    fn collect_values(&self) -> Values {
        self.values.clone()
    }

    fn emit_event(&self, event: &UiEvent) -> anyhow::Result<()> {
        let mut stdout = std::io::stdout().lock();
        serde_json::to_writer(&mut stdout, event)?;
        stdout.write_all(b"\n")?;
        stdout.flush()?;
        Ok(())
    }

    fn request_cancel(&mut self, ctx: &egui::Context) {
        if self.pending.is_some() || self.exit_code.is_some() {
            return;
        }
        if self.mode == Mode::Interactive {
            if let Err(err) = self.emit_event(&UiEvent::Cancel) {
                warn!("failed to emit cancel: {err}");
            }
        }
        self.exit_code = Some(1);
        ctx.send_viewport_cmd(ViewportCommand::Close);
    }

    fn request_submit(&mut self, action_id: &str) {
        if self.pending.is_some() || self.exit_code.is_some() {
            return;
        }
        self.form_error = None;
        let values = self.collect_values();
        match self.mode {
            Mode::Simple => {
                self.result = Some(FormResult {
                    action: action_id.to_owned(),
                    values,
                });
                self.exit_code = Some(0);
            }
            Mode::Interactive => {
                if let Err(err) = self.emit_event(&UiEvent::Submit {
                    action: action_id.to_owned(),
                    values,
                }) {
                    warn!("failed to emit submit: {err}");
                    self.form_error = Some(err.to_string());
                    return;
                }
                self.pending = Some(PendingRequest::Submit(action_id.to_owned()));
            }
        }
    }

    fn request_action(&mut self, action_id: &str) {
        if self.mode == Mode::Simple || self.pending.is_some() || self.exit_code.is_some() {
            return;
        }
        let values = self.collect_values();
        if let Err(err) = self.emit_event(&UiEvent::Action {
            action: action_id.to_owned(),
            values,
        }) {
            warn!("failed to emit action: {err}");
            self.form_error = Some(err.to_string());
            return;
        }
        self.pending = Some(PendingRequest::Action);
    }

    fn apply_field_updates(&mut self, updates: Vec<FieldUpdate>) {
        for update in updates {
            if let Some(field) = self.spec.fields.iter_mut().find(|f| f.id == update.id) {
                if let Some(error) = update.error {
                    field.error = if error.is_empty() { None } else { Some(error) };
                }
                if let Some(label) = update.label {
                    field.label = label;
                }
                if let Some(placeholder) = update.placeholder {
                    field.placeholder = Some(placeholder);
                }
                if let Some(options) = update.options {
                    field.options = options;
                }
            }
            if let Some(value) = update.value {
                self.values.insert(update.id, value);
            }
        }
    }

    fn handle_reply(&mut self, reply: UiReply, ctx: &egui::Context) {
        match reply {
            UiReply::Ok => match self.pending.take() {
                Some(PendingRequest::Submit(action)) => {
                    let values = self.collect_values();
                    self.result = Some(FormResult { action, values });
                    self.exit_code = Some(0);
                    ctx.send_viewport_cmd(ViewportCommand::Close);
                }
                Some(PendingRequest::Action) | None => {}
            },
            UiReply::Error { message } => {
                self.pending = None;
                self.form_error = Some(message);
            }
            UiReply::Update { fields } => {
                self.apply_field_updates(fields);
                if matches!(self.pending, Some(PendingRequest::Action)) {
                    self.pending = None;
                }
            }
            UiReply::Close => {
                self.pending = None;
                self.exit_code = Some(1);
                ctx.send_viewport_cmd(ViewportCommand::Close);
            }
        }
    }

    fn poll_replies(&mut self, ctx: &egui::Context) {
        let mut replies = Vec::new();
        let mut disconnected = false;
        if let Some(rx) = &self.reply_rx {
            loop {
                match rx.try_recv() {
                    Ok(reply) => replies.push(reply),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }
        if disconnected {
            self.reply_rx = None;
        }
        for reply in replies {
            self.handle_reply(reply, ctx);
        }
    }

    fn on_field_change(&mut self, id: &str, value: FieldValue) {
        self.values.insert(id.to_owned(), value.clone());
        if self.mode != Mode::Interactive {
            return;
        }
        let values = self.collect_values();
        if let Err(err) = self.emit_event(&UiEvent::Change {
            id: id.to_owned(),
            value,
            values,
        }) {
            warn!("failed to emit change: {err}");
        }
    }
}

impl eframe::App for FormApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_replies(ctx);

        if self.exit_code.is_some() {
            if self.mode == Mode::Simple {
                if let Some(result) = self.result.take() {
                    if let Err(err) = write_simple_result(&result) {
                        warn!("failed to write result: {err}");
                    }
                }
            }
            ctx.send_viewport_cmd(ViewportCommand::Close);
            return;
        }

        if ctx.input(|i| i.key_pressed(Key::Escape)) {
            self.request_cancel(ctx);
        }

        let busy = self.pending.is_some();
        let mut clicked_submit: Option<String> = None;
        let mut clicked_cancel = false;
        let mut clicked_action: Option<String> = None;
        let mut changes: Vec<(String, FieldValue)> = Vec::new();
        let mut focus_id: Option<egui::Id> = None;
        let apply_focus = !self.focus_applied;

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_enabled_ui(!busy, |ui| {
                if !self.spec.title.is_empty() {
                    ui.heading(&self.spec.title);
                    ui.add_space(8.0);
                }

                if let Some(err) = &self.form_error {
                    ui.colored_label(Color32::from_rgb(200, 80, 80), err);
                    ui.add_space(6.0);
                }

                for field in &self.spec.fields {
                    match field.field_type {
                        FieldType::Label => {
                            ui.label(&field.label);
                        }
                        FieldType::Text => {
                            ui.label(&field.label);
                            let mut text = match self.values.get(&field.id) {
                                Some(FieldValue::String(s)) => s.clone(),
                                _ => String::new(),
                            };
                            let id = egui::Id::new(("field", &field.id));
                            if field.focus && apply_focus {
                                focus_id = Some(id);
                            }
                            let mut edit = egui::TextEdit::singleline(&mut text).id(id);
                            if let Some(ph) = &field.placeholder {
                                edit = edit.hint_text(ph);
                            }
                            if field.password {
                                edit = edit.password(true);
                            }
                            let response = ui.add(edit.desired_width(f32::INFINITY));
                            if response.changed() {
                                changes.push((field.id.clone(), FieldValue::String(text)));
                            }
                            if let Some(err) = &field.error {
                                ui.colored_label(Color32::from_rgb(200, 80, 80), err);
                            }
                        }
                        FieldType::Toggle => {
                            let mut checked = match self.values.get(&field.id) {
                                Some(FieldValue::Bool(b)) => *b,
                                _ => false,
                            };
                            if ui.checkbox(&mut checked, &field.label).changed() {
                                changes.push((field.id.clone(), FieldValue::Bool(checked)));
                            }
                            if let Some(err) = &field.error {
                                ui.colored_label(Color32::from_rgb(200, 80, 80), err);
                            }
                        }
                        FieldType::Choice => {
                            ui.label(&field.label);
                            let current = match self.values.get(&field.id) {
                                Some(FieldValue::String(s)) => s.clone(),
                                _ => field
                                    .options
                                    .first()
                                    .map(|o| o.id().to_owned())
                                    .unwrap_or_default(),
                            };
                            let label = field
                                .options
                                .iter()
                                .find(|o| o.id() == current)
                                .map(|o| o.label().to_owned())
                                .unwrap_or_else(|| current.clone());
                            let mut selected = current.clone();
                            egui::ComboBox::from_id_salt(&field.id)
                                .selected_text(label)
                                .width(ui.available_width())
                                .show_ui(ui, |ui| {
                                    for opt in &field.options {
                                        ui.selectable_value(
                                            &mut selected,
                                            opt.id().to_owned(),
                                            opt.label(),
                                        );
                                    }
                                });
                            if selected != current {
                                changes.push((field.id.clone(), FieldValue::String(selected)));
                            }
                            if let Some(err) = &field.error {
                                ui.colored_label(Color32::from_rgb(200, 80, 80), err);
                            }
                        }
                    }
                    ui.add_space(6.0);
                }

                if let Some(id) = focus_id {
                    ui.memory_mut(|m| m.request_focus(id));
                }

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    for action in &self.spec.actions {
                        let label = if action.label.is_empty() {
                            action.id.as_str()
                        } else {
                            action.label.as_str()
                        };
                        let text = if action.primary {
                            RichText::new(label).strong()
                        } else {
                            RichText::new(label)
                        };
                        if ui.button(text).clicked() {
                            if action.submit {
                                clicked_submit = Some(action.id.clone());
                            } else if action.id == "cancel" || label.eq_ignore_ascii_case("cancel")
                            {
                                clicked_cancel = true;
                            } else {
                                clicked_action = Some(action.id.clone());
                            }
                        }
                    }
                });
            });

            if busy {
                ui.add_space(4.0);
                ui.spinner();
            }
        });

        if focus_id.is_some() {
            self.focus_applied = true;
        }

        for (id, value) in changes {
            self.on_field_change(&id, value);
        }

        if clicked_cancel {
            self.request_cancel(ctx);
        } else if let Some(id) = clicked_submit {
            self.request_submit(&id);
        } else if let Some(id) = clicked_action {
            self.request_action(&id);
        }

        if !busy && ctx.input(|i| i.key_pressed(Key::Enter)) {
            if let Some(action) = self
                .spec
                .actions
                .iter()
                .find(|a| a.submit && a.primary)
                .or_else(|| self.spec.actions.iter().find(|a| a.submit))
            {
                let id = action.id.clone();
                self.request_submit(&id);
            }
        }
    }

    fn on_exit(&mut self) {
        debug!("lumalla-ui exiting with {:?}", self.exit_code);
    }
}

fn write_simple_result(result: &FormResult) -> anyhow::Result<()> {
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer(&mut stdout, result)?;
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    Ok(())
}

fn spawn_stdin_reader(tx: Sender<UiReply>) {
    thread::Builder::new()
        .name(String::from("lumalla-ui-stdin"))
        .spawn(move || {
            let stdin = std::io::stdin();
            let mut reader = BufReader::new(stdin.lock());
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
                        match serde_json::from_str::<UiReply>(trimmed) {
                            Ok(reply) => {
                                if tx.send(reply).is_err() {
                                    break;
                                }
                            }
                            Err(err) => warn!("invalid UiReply from config: {err} ({trimmed})"),
                        }
                    }
                    Err(err) => {
                        warn!("stdin read error: {err}");
                        break;
                    }
                }
            }
        })
        .ok();
}

/// Run the form UI; returns a process exit code.
pub fn run_form(spec: FormSpec, mode: Mode) -> anyhow::Result<ExitCode> {
    let title = if spec.title.is_empty() {
        String::from("Lumalla")
    } else {
        spec.title.clone()
    };

    let (reply_tx, reply_rx) = mpsc::channel();
    let reply_rx = match mode {
        Mode::Interactive => {
            spawn_stdin_reader(reply_tx);
            Some(reply_rx)
        }
        Mode::Simple => None,
    };

    let exit_holder = ExitHolder::default();
    let exit_flag = exit_holder.clone_slot();

    let viewport = ViewportBuilder::default()
        .with_title(title)
        .with_inner_size([420.0, 280.0])
        .with_resizable(true);

    let native_options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    let app = FormApp::new(spec, mode, reply_rx);
    match eframe::run_native(
        "lumalla-ui",
        native_options,
        Box::new(move |_cc| {
            Ok(Box::new(ExitTrackingApp {
                inner: app,
                exit_flag,
            }))
        }),
    ) {
        Ok(()) => {}
        Err(err) => {
            anyhow::bail!("eframe failed to start: {err}");
        }
    }

    let code = exit_holder.take().unwrap_or(1);
    Ok(ExitCode::from(code))
}

#[derive(Default)]
struct ExitHolder {
    slot: std::sync::Arc<std::sync::Mutex<Option<u8>>>,
}

impl ExitHolder {
    fn clone_slot(&self) -> std::sync::Arc<std::sync::Mutex<Option<u8>>> {
        self.slot.clone()
    }

    fn take(&self) -> Option<u8> {
        self.slot.lock().ok().and_then(|mut g| g.take())
    }
}

struct ExitTrackingApp {
    inner: FormApp,
    exit_flag: std::sync::Arc<std::sync::Mutex<Option<u8>>>,
}

impl eframe::App for ExitTrackingApp {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.inner.update(ctx, frame);
        if let Some(code) = self.inner.exit_code {
            if let Ok(mut g) = self.exit_flag.lock() {
                *g = Some(code);
            }
        }
    }

    fn on_exit(&mut self) {
        if let Some(code) = self.inner.exit_code {
            if let Ok(mut g) = self.exit_flag.lock() {
                *g = Some(code);
            }
        }
        self.inner.on_exit();
    }
}
