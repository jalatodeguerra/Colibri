//! The message loop and the handlers.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::error::Error;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use colibri_check::DEFAULT_STACK_BUDGET;
use colibri_check::package::{DroppedKind, Loaded, normalize};
use colibri_syntax::{Severity, SourceMap, format_source};
use lsp_server::{Connection, ErrorCode, Message, Notification, Request, RequestId, Response};
use lsp_types::notification::{
    DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument, DidSaveTextDocument,
    LogMessage, Notification as _, PublishDiagnostics, ShowMessage,
};
use lsp_types::request::{
    Completion, DocumentSymbolRequest, Formatting, GotoDefinition, HoverRequest,
    PrepareRenameRequest, References, Rename, Request as _,
};
use lsp_types::{
    CompletionOptions, CompletionResponse, Diagnostic, DiagnosticRelatedInformation,
    DiagnosticSeverity, DiagnosticTag, DocumentSymbolResponse, GotoDefinitionResponse, Hover,
    HoverContents, HoverProviderCapability, InitializeParams, Location, LogMessageParams,
    MarkupContent, MarkupKind, MessageType, OneOf, PublishDiagnosticsParams, RenameOptions,
    ServerCapabilities, ServerInfo, ShowMessageParams, TextDocumentSyncCapability,
    TextDocumentSyncKind, TextEdit, Uri, WorkDoneProgressOptions, WorkspaceEdit,
};
use serde_json::Value;

use crate::analysis::{
    Analysis, Overlay, Pending, Reader, Settings, Spec, analyze, analyze_in, canon, coverage,
};
use crate::completion::{self, Env, Kind};
use crate::convert::{LineIndex, path_to_uri, uri_to_path};
use crate::rename;
use crate::resolve::{DefInfo, DefKind, Resolution, Target, resolve};
use crate::symbols::document_symbols;

type BoxError = Box<dyn Error + Send + Sync>;

/// How long the server waits after a change to a document before it analyses the package, so
/// that a burst of keystrokes costs one analysis. A request that needs an analysis does not
/// wait for it (see [`Config::debounce`]).
pub const DEBOUNCE: Duration = Duration::from_millis(150);

/// Settings of the server that the client does not send.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// How much native stack checking may use (see `colibri_check::check_with_stack_budget`).
    pub stack_budget: usize,
    /// The quiet period after a `didChange` before the diagnostics are computed; a newer change
    /// restarts it, and only the newest text is analysed. [`DEBOUNCE`] by default. Requests
    /// (hover, completion, definition, references, rename, symbols) analyse pending changes at
    /// once instead of waiting, so an answer never describes an older text. Tests set it very
    /// long (nothing is analysed but by a request) or zero (every change is analysed as soon as
    /// the incoming queue is empty).
    pub debounce: Duration,
    /// How long the server waits for `exit` after it answered `shutdown` before it gives up and
    /// ends with an error (so the process does not hang on a client that never says `exit`).
    /// [`EXIT_WAIT`] by default.
    pub exit_wait: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            stack_budget: DEFAULT_STACK_BUDGET,
            debounce: DEBOUNCE,
            exit_wait: EXIT_WAIT,
        }
    }
}

/// Serves the protocol on stdin and stdout until the client asks to exit. `Err` when the
/// session did not end the way the protocol says (`exit` without `shutdown`, or no `exit` after
/// `shutdown`); the caller then ends the process without waiting for the stdin reader, which
/// may be blocked on a client that never closes the pipe.
pub fn run_stdio(config: Config) -> Result<(), BoxError> {
    let (connection, io_threads) = Connection::stdio();
    serve(connection, config)?;
    io_threads.join()?;
    Ok(())
}

/// Serves the protocol on `connection` until the client asks to exit. `Ok` after `shutdown`
/// followed by `exit` and when the client just went away; `Err` after an `exit` that came
/// without `shutdown` (the protocol's exit code is 1), after a `shutdown` whose `exit` did not
/// come within [`Config::exit_wait`], and after a connection that closed in between.
pub fn serve(connection: Connection, config: Config) -> Result<(), BoxError> {
    let capabilities = ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
        document_formatting_provider: Some(OneOf::Left(true)),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        definition_provider: Some(OneOf::Left(true)),
        document_symbol_provider: Some(OneOf::Left(true)),
        references_provider: Some(OneOf::Left(true)),
        rename_provider: Some(OneOf::Right(RenameOptions {
            prepare_provider: Some(true),
            work_done_progress_options: WorkDoneProgressOptions::default(),
        })),
        completion_provider: Some(CompletionOptions {
            trigger_characters: Some(vec![".".to_string()]),
            ..CompletionOptions::default()
        }),
        ..ServerCapabilities::default()
    };
    let result = serde_json::json!({
        "capabilities": capabilities,
        "serverInfo": ServerInfo { name: "colibri".to_string(), version: Some(env!("CARGO_PKG_VERSION").to_string()) },
    });
    let (id, params) = connection.initialize_start()?;
    connection.initialize_finish(id, result)?;
    let params: InitializeParams = serde_json::from_value(params)?;
    let mut server = Server::new(&connection, config, &params);
    server.run().map_err(Into::into)
}

/// Whether the file at `path` is Colibri source (`*.colibri`): the only kind of document a
/// request is answered for.
fn is_source(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "colibri")
}

/// The requests that are about one document, which has its `uri` in `textDocument`.
fn is_document_request(method: &str) -> bool {
    [
        Formatting::METHOD,
        HoverRequest::METHOD,
        GotoDefinition::METHOD,
        DocumentSymbolRequest::METHOD,
        Completion::METHOD,
        References::METHOD,
        PrepareRenameRequest::METHOD,
        Rename::METHOD,
    ]
    .contains(&method)
}

/// How long the server waits for `exit` after answering `shutdown` by default.
pub const EXIT_WAIT: Duration = Duration::from_secs(30);

/// The method of the cancellation notification.
const CANCEL_REQUEST: &str = "$/cancelRequest";

/// An open document.
struct Doc {
    uri: Uri,
    text: String,
}

struct Server<'c> {
    connection: &'c Connection,
    settings: Settings,
    /// Open documents by normalised path.
    docs: HashMap<PathBuf, Doc>,
    /// Text of the open documents, as the loader reads it.
    buffers: HashMap<PathBuf, String>,
    /// The latest analysis that includes each file.
    latest: HashMap<PathBuf, Rc<Analysis>>,
    /// The latest analysis of each open document whose program had types, with the text it
    /// was made from: what completion falls back on while the file does not check.
    good: HashMap<PathBuf, (String, Rc<Analysis>)>,
    /// The files each open document's package published diagnostics for.
    published: HashMap<PathBuf, HashSet<PathBuf>>,
    /// Interface files already reported as unreadable, so the message is sent once.
    reported: HashSet<PathBuf>,
    /// Messages that arrived and have not been handled yet, oldest first. The loop moves
    /// everything that is waiting on the connection here before it starts a request, so it can
    /// see a cancellation that is queued behind the request.
    inbox: VecDeque<Message>,
    /// Open documents whose text changed since they were last analysed, in the order of their
    /// first change.
    dirty: Vec<PathBuf>,
    /// When the quiet period of the newest change ends; `None` when nothing is pending.
    due: Option<Instant>,
    debounce: Duration,
    exit_wait: Duration,
}

impl<'c> Server<'c> {
    fn new(connection: &'c Connection, config: Config, params: &InitializeParams) -> Self {
        #[allow(deprecated)]
        let root = params
            .workspace_folders
            .as_ref()
            .and_then(|f| f.first())
            .map(|f| uri_to_path(&f.uri))
            .or_else(|| params.root_uri.as_ref().map(uri_to_path))
            .map(|p| normalize(&p));
        let interfaces = interface_paths(params.initialization_options.as_ref(), root.as_deref());
        let root_shown = root.clone();
        let root = root.map(|r| canon(&r));
        Server {
            connection,
            settings: Settings {
                stack_budget: config.stack_budget,
                interfaces,
                root,
                root_shown,
            },
            docs: HashMap::new(),
            buffers: HashMap::new(),
            latest: HashMap::new(),
            good: HashMap::new(),
            published: HashMap::new(),
            reported: HashSet::new(),
            inbox: VecDeque::new(),
            dirty: Vec::new(),
            due: None,
            debounce: config.debounce,
            exit_wait: config.exit_wait,
        }
    }

    // ---- the message loop ----

    /// Handles messages in order until the client exits or the connection closes. `Err` for the
    /// ways the protocol's end of a session goes wrong (see [`serve`]). The server is
    /// single-threaded: a change only marks the document dirty, and the analysis runs when the
    /// quiet period ends with nothing else waiting, or at once when a request needs it.
    fn run(&mut self) -> Result<(), String> {
        loop {
            self.drain();
            let Some(message) = self.inbox.pop_front().or_else(|| self.wait()) else {
                return Ok(());
            };
            match message {
                Message::Notification(note) if note.method == "exit" => {
                    return Err("`exit` arrived without `shutdown`".to_string());
                }
                Message::Notification(note) => self.notification(note),
                Message::Request(req) if req.method == "shutdown" => {
                    return self.shutdown(req);
                }
                Message::Request(req) => self.start_request(req),
                Message::Response(_) => {}
            }
        }
    }

    /// Moves what is waiting on the connection into the inbox, without blocking.
    fn drain(&mut self) {
        while let Ok(message) = self.connection.receiver.try_recv() {
            self.inbox.push_back(message);
        }
    }

    /// Blocks for the next message; while a change is pending, only until its quiet period ends,
    /// and then the analysis runs and the wait goes on. `None` when the connection is closed.
    fn wait(&mut self) -> Option<Message> {
        loop {
            let Some(due) = self.due else {
                return self.connection.receiver.recv().ok();
            };
            let now = Instant::now();
            if now < due {
                match self.connection.receiver.recv_timeout(due - now) {
                    Ok(message) => return Some(message),
                    Err(e) if e.is_timeout() => {}
                    Err(_) => return None,
                }
            }
            // A message may have come in as the time ran out; it is handled first.
            self.drain();
            if !self.inbox.is_empty() {
                return self.inbox.pop_front();
            }
            self.flush();
        }
    }

    /// Answers `shutdown` and waits for `exit`, at most [`Config::exit_wait`] for each message.
    /// Pending changes are dropped: nothing is analysed or published after the client asked to
    /// stop.
    fn shutdown(&mut self, req: Request) -> Result<(), String> {
        self.dirty.clear();
        self.due = None;
        self.send(Response::new_ok(req.id, ()));
        loop {
            let message = match self.inbox.pop_front() {
                Some(m) => m,
                None => match self.connection.receiver.recv_timeout(self.exit_wait) {
                    Ok(m) => m,
                    Err(e) if e.is_timeout() => {
                        return Err("waiting for the `exit` notification timed out".to_string());
                    }
                    Err(_) => {
                        return Err(
                            "the connection closed before the `exit` notification".to_string()
                        );
                    }
                },
            };
            match message {
                Message::Notification(note) if note.method == "exit" => return Ok(()),
                Message::Request(req) => self.respond_err(
                    req.id,
                    ErrorCode::InvalidRequest,
                    "the server is shutting down".to_string(),
                ),
                _ => {}
            }
        }
    }

    /// Whether the client has cancelled request `id`: a `$/cancelRequest` for it is waiting.
    fn cancelled(&mut self, id: &RequestId) -> bool {
        self.drain();
        self.inbox.iter().any(|m| match m {
            Message::Notification(n) if n.method == CANCEL_REQUEST => n
                .params
                .get("id")
                .and_then(|v| serde_json::from_value::<RequestId>(v.clone()).ok())
                .is_some_and(|c| &c == id),
            _ => false,
        })
    }

    /// Starts a request: skipped with `RequestCancelled` if the client already cancelled it;
    /// otherwise pending changes are analysed first (formatting only needs the text, which is
    /// always current), so the answer describes the document as it is now.
    fn start_request(&mut self, req: Request) {
        let needs_analysis = req.method != Formatting::METHOD;
        if self.cancelled(&req.id) || (needs_analysis && self.flush_then_cancelled(&req.id)) {
            self.respond_err(
                req.id,
                ErrorCode::RequestCanceled,
                "the request was cancelled".to_string(),
            );
            return;
        }
        self.request(req);
    }

    /// Analyses what is pending, and reports whether the request was cancelled meanwhile.
    fn flush_then_cancelled(&mut self, id: &RequestId) -> bool {
        self.flush();
        self.cancelled(id)
    }

    /// Analyses every document that changed since it was last analysed. Documents of one
    /// package are analysed once between them: the analysis of the first covers the others that
    /// it loaded, which take it over without a second analysis or a second publish.
    fn flush(&mut self) {
        self.due = None;
        let mut pending = std::mem::take(&mut self.dirty);
        pending.retain(|p| self.docs.contains_key(p));
        while !pending.is_empty() {
            let path = pending.remove(0);
            let analysis = self.check(&path);
            let (covered, rest): (Vec<PathBuf>, Vec<PathBuf>) = pending
                .into_iter()
                .partition(|p| analysis.file_of(p).is_some());
            pending = rest;
            for other in covered {
                self.adopt(&other, &analysis);
            }
        }
    }

    /// Records `analysis` (made for another document of the same package, whose result was
    /// already published) as the analysis of `doc` too.
    fn adopt(&mut self, doc: &Path, analysis: &Rc<Analysis>) {
        self.remember_good(doc, analysis);
        let files: HashSet<PathBuf> = analysis.files().into_iter().map(|(_, p)| p).collect();
        self.published.insert(doc.to_path_buf(), files);
    }

    fn remember_good(&mut self, doc: &Path, analysis: &Rc<Analysis>) {
        if !analysis.insight.facts().is_empty()
            && let Some(text) = self.buffers.get(doc)
        {
            self.good
                .insert(doc.to_path_buf(), (text.clone(), analysis.clone()));
        }
    }

    /// Notes that `path` changed: it is analysed when the quiet period after the newest change
    /// ends, or when a request needs it.
    fn schedule(&mut self, path: PathBuf) {
        if !self.dirty.contains(&path) {
            self.dirty.push(path);
        }
        self.due = Some(Instant::now() + self.debounce);
    }

    /// Analyses `path` now; a pending analysis of it is then not needed.
    fn check_now(&mut self, path: &Path) {
        self.dirty.retain(|p| p != path);
        if self.dirty.is_empty() {
            self.due = None;
        }
        self.check(path);
    }

    fn send(&self, message: impl Into<Message>) {
        // A closed connection ends the loop on the next receive.
        let _ = self.connection.sender.send(message.into());
    }

    fn notify<N: lsp_types::notification::Notification>(&self, params: N::Params) {
        self.send(Notification::new(N::METHOD.to_string(), params));
    }

    fn log(&self, message: String) {
        self.notify::<LogMessage>(LogMessageParams {
            typ: MessageType::WARNING,
            message,
        });
    }

    // ---- notifications ----

    fn notification(&mut self, note: Notification) {
        match note.method.as_str() {
            DidOpenTextDocument::METHOD => {
                if let Ok(p) =
                    serde_json::from_value::<lsp_types::DidOpenTextDocumentParams>(note.params)
                {
                    for path in self.set_text(p.text_document.uri, p.text_document.text) {
                        self.check_now(&path);
                    }
                }
            }
            DidChangeTextDocument::METHOD => {
                if let Ok(p) =
                    serde_json::from_value::<lsp_types::DidChangeTextDocumentParams>(note.params)
                {
                    // Full synchronisation: the last change is the whole text.
                    if let Some(change) = p.content_changes.into_iter().last() {
                        for path in self.set_text(p.text_document.uri, change.text) {
                            self.schedule(path);
                        }
                    }
                }
            }
            DidSaveTextDocument::METHOD => {
                if let Ok(p) =
                    serde_json::from_value::<lsp_types::DidSaveTextDocumentParams>(note.params)
                {
                    // The buffer is what was saved; a save can still change what imports read.
                    if let Some(doc) = self.docs.get(&key(&p.text_document.uri)) {
                        let (uri, text) = (doc.uri.clone(), doc.text.clone());
                        for path in self.set_text(uri, text) {
                            self.check_now(&path);
                        }
                    }
                }
            }
            DidCloseTextDocument::METHOD => {
                if let Ok(p) =
                    serde_json::from_value::<lsp_types::DidCloseTextDocumentParams>(note.params)
                {
                    self.close(&key(&p.text_document.uri));
                }
            }
            _ => {}
        }
    }

    /// Records the new text of a document and returns the documents to analyse for it (the
    /// document itself when it is Colibri source); the caller decides when.
    fn set_text(&mut self, uri: Uri, text: String) -> Vec<PathBuf> {
        let path = key(&uri);
        self.buffers.insert(path.clone(), text.clone());
        self.docs.insert(path.clone(), Doc { uri, text });
        if is_source(&path) {
            return vec![path];
        }
        // A `colibri.toml` (or any other file) the editor opened is not Colibri source: it is
        // never parsed as such. It is a buffer the loader reads when it reads the package, so
        // the open Colibri documents are the ones to analyse again.
        self.docs.keys().filter(|p| is_source(p)).cloned().collect()
    }

    fn close(&mut self, path: &Path) {
        self.dirty.retain(|p| p != path);
        self.docs.remove(path);
        self.buffers.remove(path);
        self.good.remove(path);
        let files = self.published.remove(path).unwrap_or_default();
        let still: HashSet<&PathBuf> = self.published.values().flatten().collect();
        for file in files {
            if !still.contains(&file) {
                self.publish(&file, Vec::new());
                self.latest.remove(&file);
            }
        }
        // Other open documents may have imported this one: they see the file on disk now.
        let others: Vec<PathBuf> = self.docs.keys().cloned().collect();
        for other in others {
            if self
                .latest
                .get(&other)
                .is_some_and(|a| a.file_of(path).is_some())
            {
                self.check_now(&other);
            }
        }
    }

    /// Checks the package of `doc` and publishes what it found.
    fn check(&mut self, doc: &Path) -> Rc<Analysis> {
        let overlay = Overlay::new(&self.buffers);
        let (analysis, problems) = analyze(doc, &overlay, &self.settings);
        for (path, message) in problems {
            if self.reported.insert(path.clone()) {
                self.log(format!(
                    "colibri: cannot use the interface file {}: {message}",
                    path.display()
                ));
            }
        }
        let analysis = Rc::new(analysis);
        self.remember_good(doc, &analysis);
        let files = analysis.files();
        let mut now = HashSet::new();
        for (file, path) in &files {
            let diagnostics = self.convert_diagnostics(&analysis, *file);
            self.publish(path, diagnostics);
            self.latest.insert(path.clone(), analysis.clone());
            now.insert(path.clone());
        }
        let before = self.published.insert(doc.to_path_buf(), now.clone());
        let still: HashSet<PathBuf> = self.published.values().flatten().cloned().collect();
        for gone in before.into_iter().flatten().filter(|p| !now.contains(p)) {
            if !still.contains(&gone) {
                self.publish(&gone, Vec::new());
                self.latest.remove(&gone);
            }
        }
        analysis
    }

    /// The URI the client knows a file by: the one it opened it with, else a path under the
    /// workspace folder as the client wrote it, else the canonical path.
    fn uri_of(&self, path: &Path) -> Option<Uri> {
        if let Some(doc) = self.docs.get(path) {
            return Some(doc.uri.clone());
        }
        if let (Some(root), Some(shown)) = (&self.settings.root, &self.settings.root_shown)
            && let Ok(rest) = path.strip_prefix(root)
        {
            return path_to_uri(&shown.join(rest));
        }
        path_to_uri(path)
    }

    fn publish(&self, path: &Path, diagnostics: Vec<Diagnostic>) {
        let Some(uri) = self.uri_of(path) else { return };
        self.notify::<PublishDiagnostics>(PublishDiagnosticsParams {
            uri,
            diagnostics,
            version: None,
        });
    }

    fn convert_diagnostics(
        &self,
        analysis: &Analysis,
        file: colibri_syntax::FileId,
    ) -> Vec<Diagnostic> {
        let sources = &analysis.sources;
        let location = |span: colibri_syntax::Span| -> Option<Location> {
            if !sources.contains(span.file) {
                return None;
            }
            let path = analysis.path_of(span.file);
            let index = LineIndex::new(sources.text(span.file));
            Some(Location::new(
                self.uri_of(&path)?,
                index.range(span.start as usize, span.end as usize),
            ))
        };
        analysis
            .diagnostics
            .iter()
            .filter(|d| d.span.file == file)
            .filter_map(|d| {
                let loc = location(d.span)?;
                let related: Vec<DiagnosticRelatedInformation> = d
                    .labels
                    .iter()
                    .filter_map(|(span, text)| {
                        Some(DiagnosticRelatedInformation {
                            location: location(*span)?,
                            message: text.clone(),
                        })
                    })
                    .collect();
                let mut message = d.message.clone();
                if let Some(hint) = &d.hint {
                    message.push_str("\nhint: ");
                    message.push_str(hint);
                }
                Some(Diagnostic {
                    range: loc.range,
                    severity: Some(match d.severity {
                        Severity::Error => DiagnosticSeverity::ERROR,
                        Severity::Warning => DiagnosticSeverity::WARNING,
                    }),
                    source: Some("colibri".to_string()),
                    // Unused code is shown faded, deprecated names struck through.
                    tags: {
                        let warning = d.severity == Severity::Warning;
                        let mut tags = Vec::new();
                        if warning
                            && (d.message.starts_with("unused variable")
                                || d.message.starts_with("unused import"))
                        {
                            tags.push(DiagnosticTag::UNNECESSARY);
                        }
                        if warning && is_deprecation_warning(&d.message) {
                            tags.push(DiagnosticTag::DEPRECATED);
                        }
                        (!tags.is_empty()).then_some(tags)
                    },
                    message,
                    related_information: (!related.is_empty()).then_some(related),
                    ..Diagnostic::default()
                })
            })
            .collect()
    }

    // ---- requests ----

    fn request(&mut self, req: Request) {
        let Request { id, method, params } = req;
        // A document that is not Colibri source (a `colibri.toml` the editor opened) is a
        // buffer for the loader and nothing else: no request about it parses it, or looks
        // at it at all.
        if is_document_request(&method)
            && params
                .pointer("/textDocument/uri")
                .and_then(|u| serde_json::from_value::<Uri>(u.clone()).ok())
                .is_some_and(|uri| !is_source(&key(&uri)))
        {
            self.send(Response::new_ok(id, Value::Null));
            return;
        }
        let result: Result<Value, (ErrorCode, String)> = match method.as_str() {
            Formatting::METHOD => self.reply(params, Self::formatting),
            HoverRequest::METHOD => self.reply(params, Self::hover),
            GotoDefinition::METHOD => self.reply(params, Self::definition),
            DocumentSymbolRequest::METHOD => self.reply(params, Self::document_symbol),
            Completion::METHOD => self.reply(params, Self::completion),
            References::METHOD => self.reply_refusable(params, Self::references),
            PrepareRenameRequest::METHOD => self.reply_refusable(params, Self::prepare_rename),
            Rename::METHOD => self.reply_refusable(params, Self::rename),
            // For the tests that bound the work a request does: what this server has read.
            "colibri/counters" => {
                let (files_read, analyses) = crate::analysis::counters();
                Ok(serde_json::json!({"filesRead": files_read, "analyses": analyses}))
            }
            _ => {
                self.respond_err(
                    id,
                    ErrorCode::MethodNotFound,
                    format!("unknown request `{method}`"),
                );
                return;
            }
        };
        match result {
            Ok(value) => self.send(Response::new_ok(id, value)),
            Err((code, message)) => self.respond_err(id, code, message),
        }
    }

    /// The open document at `path`, if it is Colibri source.
    fn source_doc(&self, path: &Path) -> Option<&Doc> {
        if !is_source(path) {
            return None;
        }
        self.docs.get(path)
    }

    fn respond_err(&self, id: RequestId, code: ErrorCode, message: String) {
        self.send(Response::new_err(id, code as i32, message));
    }

    fn reply<P: serde::de::DeserializeOwned, R: serde::Serialize>(
        &self,
        params: Value,
        f: fn(&Self, P) -> R,
    ) -> Result<Value, (ErrorCode, String)> {
        let invalid = |e: serde_json::Error| (ErrorCode::InvalidParams, e.to_string());
        let params = serde_json::from_value(params).map_err(invalid)?;
        serde_json::to_value(f(self, params)).map_err(invalid)
    }

    /// Like [`Server::reply`] for a request that can be refused, with the reason (a rename of
    /// something that cannot be renamed): the client shows the message to the user.
    fn reply_refusable<P: serde::de::DeserializeOwned, R: serde::Serialize>(
        &self,
        params: Value,
        f: fn(&Self, P) -> Result<R, String>,
    ) -> Result<Value, (ErrorCode, String)> {
        let invalid = |e: serde_json::Error| (ErrorCode::InvalidParams, e.to_string());
        let params = serde_json::from_value(params).map_err(invalid)?;
        let answer = f(self, params).map_err(|reason| (ErrorCode::RequestFailed, reason))?;
        serde_json::to_value(answer).map_err(invalid)
    }

    fn formatting(&self, p: lsp_types::DocumentFormattingParams) -> Option<Vec<TextEdit>> {
        let path = key(&p.text_document.uri);
        let doc = self.source_doc(&path)?;
        // A file with syntax errors is left alone.
        let formatted = format_source(&path.to_string_lossy(), &doc.text).ok()?;
        if formatted == doc.text {
            return Some(Vec::new());
        }
        let range = LineIndex::new(&doc.text).whole();
        Some(vec![TextEdit::new(range, formatted)])
    }

    fn document_symbol(
        &self,
        p: lsp_types::DocumentSymbolParams,
    ) -> Option<DocumentSymbolResponse> {
        let path = key(&p.text_document.uri);
        let doc = self.source_doc(&path)?;
        Some(DocumentSymbolResponse::Nested(document_symbols(&doc.text)))
    }

    fn completion(&self, p: lsp_types::CompletionParams) -> Option<CompletionResponse> {
        let at = &p.text_document_position;
        let path = key(&at.text_document.uri);
        let doc = self.source_doc(&path)?;
        let offset = LineIndex::new(&doc.text).offset(at.position);
        let none = || Some(CompletionResponse::Array(Vec::new()));
        let Some(ctx) = completion::context(&doc.text, offset) else {
            return none();
        };
        let good = self.good.get(&path).map(|(t, a)| (t.as_str(), &**a));
        let items = match &ctx.kind {
            Kind::Member {
                receiver,
                line_start,
            } => {
                // The file does not parse at the cursor: check it with the dot and the word
                // taken out, so that the receiver is an expression of its own.
                let repaired = completion::repair(&doc.text, &ctx)?;
                let mut buffers = self.buffers.clone();
                buffers.insert(path.clone(), repaired.clone());
                let (analysis, _) = analyze(&path, &Overlay::new(&buffers), &self.settings);
                let env = Env {
                    path: &path,
                    analysis: &analysis,
                    text: &repaired,
                    good,
                };
                completion::member_items(&env, *receiver, *line_start)
            }
            Kind::DotVariant | Kind::Ident => {
                let analysis = self.latest.get(&path)?;
                let env = Env {
                    path: &path,
                    analysis,
                    text: &doc.text,
                    good,
                };
                completion::ident_items(&env, offset, &ctx)
            }
        };
        Some(CompletionResponse::Array(items))
    }

    /// Every way the files around a document are read (see [`coverage`]), what every name
    /// resolves to in each reading that can hold the name at the cursor, and the meaning of
    /// the name at the cursor (`None` where there is no name). What references and rename
    /// work from.
    fn package_at(
        &self,
        at: &lsp_types::TextDocumentPositionParams,
    ) -> Result<Option<Package>, String> {
        let path = key(&at.text_document.uri);
        if self.source_doc(&path).is_none() {
            return Ok(None);
        }
        let overlay = Overlay::new(&self.buffers);
        let reader = Reader::new(&overlay, &self.settings);
        let first = Rc::new(analyze_in(&reader, &path, true));
        let mut pkg = Package::new(vec![first.clone()], &path, at.position);
        // A local variable is used only where it is declared: its own file's analysis
        // answers, with no look at the rest of the package.
        let Some(found) = &mut pkg else {
            return Ok(None);
        };
        if found.is_local() {
            return Ok(pkg);
        }
        let coverage = coverage(&first, &path, &reader)?;
        found.extend(coverage, &self.settings);
        Ok(pkg)
    }

    fn location(&self, analysis: &Analysis, span: colibri_syntax::Span) -> Option<Location> {
        if !analysis.sources.contains(span.file) {
            return None;
        }
        let path = analysis.path_of(span.file);
        let index = LineIndex::new(analysis.sources.text(span.file));
        Some(Location::new(
            self.uri_of(&path)?,
            index.range(span.start as usize, span.end as usize),
        ))
    }

    fn references(&self, p: lsp_types::ReferenceParams) -> Result<Option<Vec<Location>>, String> {
        let Some(pkg) = self.package_at(&p.text_document_position)? else {
            return Ok(None);
        };
        let Some(what) = &pkg.what else {
            return Ok(None);
        };
        if let Some(why) = &pkg.incomplete {
            self.notify::<ShowMessage>(ShowMessageParams {
                typ: MessageType::WARNING,
                message: format!("colibri: the references may be incomplete: {why}"),
            });
        }
        let include = p.context.include_declaration;
        let mut seen: HashSet<(String, u32, u32, u32, u32)> = HashSet::new();
        let mut out: Vec<Location> = Vec::new();
        for (analysis, res) in &pkg.parts {
            for o in res
                .occs
                .iter()
                .filter(|o| rename::meaning(analysis, &o.target) == *what)
            {
                if (include || !o.decl)
                    && let Some(loc) = self.location(analysis, o.span)
                    && seen.insert((
                        loc.uri.as_str().to_string(),
                        loc.range.start.line,
                        loc.range.start.character,
                        loc.range.end.line,
                        loc.range.end.character,
                    ))
                {
                    out.push(loc);
                }
            }
        }
        out.sort_by_key(|l| {
            (
                l.uri.as_str().to_string(),
                l.range.start.line,
                l.range.start.character,
            )
        });
        Ok(Some(out))
    }

    fn prepare_rename(
        &self,
        p: lsp_types::TextDocumentPositionParams,
    ) -> Result<Option<lsp_types::PrepareRenameResponse>, String> {
        let path = key(&p.text_document.uri);
        let Some(pkg) = self.package_at(&p)? else {
            return Ok(None);
        };
        let Some(what) = &pkg.what else {
            return Err("there is no name to rename here".to_string());
        };
        pkg.refuse_if_broken(&path)?;
        let (analysis, res) = &pkg.parts[0];
        let at = pkg.at.expect("a name has an occurrence");
        let plan = rename::plan(
            analysis,
            res,
            at,
            &pkg.editable,
            self.settings.root.as_deref(),
        )?;
        pkg.refuse_if_incomplete(plan.info)?;
        pkg.refuse_if_left_out(plan.info, self.settings.root.as_deref())?;
        for (a, r) in &pkg.parts[1..] {
            if let Some(o) = rename::occ_of_meaning(a, r, what) {
                rename::plan(a, r, o, &pkg.editable, self.settings.root.as_deref())?;
            }
        }
        let span = res.occs[at].span;
        let range = LineIndex::new(analysis.sources.text(span.file))
            .range(span.start as usize, span.end as usize);
        Ok(Some(
            lsp_types::PrepareRenameResponse::RangeWithPlaceholder {
                range,
                placeholder: plan.info.name.clone(),
            },
        ))
    }

    fn rename(&self, p: lsp_types::RenameParams) -> Result<Option<WorkspaceEdit>, String> {
        let Some(mut pkg) = self.package_at(&p.text_document_position)? else {
            return Ok(None);
        };
        let Some(what) = pkg.what.clone() else {
            return Err("there is no name to rename here".to_string());
        };
        let path = key(&p.text_document_position.text_document.uri);
        pkg.refuse_if_broken(&path)?;
        let (analysis, res) = &pkg.parts[0];
        let plan = rename::plan(
            analysis,
            res,
            pkg.at.expect("a name has an occurrence"),
            &pkg.editable,
            self.settings.root.as_deref(),
        )?;
        pkg.refuse_if_incomplete(plan.info)?;
        pkg.refuse_if_left_out(plan.info, self.settings.root.as_deref())?;
        rename::check_new_name(plan.info, &p.new_name)?;
        let old_name = plan.info.name.clone();
        if p.new_name == old_name {
            return Ok(Some(WorkspaceEdit::default()));
        }
        // The edits of every reading that has the name, by canonical path (the same text and
        // the same span in each reading that reads the file).
        let mut per_analysis: Vec<Option<Vec<rename::Edit>>> = Vec::new();
        let mut by_place: HashMap<(PathBuf, u32, u32), String> = HashMap::new();
        for (a, r) in &pkg.parts {
            let Some(o) = rename::occ_of_meaning(a, r, &what) else {
                per_analysis.push(None);
                continue;
            };
            let plan = rename::plan(a, r, o, &pkg.editable, self.settings.root.as_deref())?;
            let edits = rename::edits(a, r, &plan, &p.new_name)?;
            for e in &edits {
                let key = (rename::path_of(a, e.span.file), e.span.start, e.span.end);
                by_place.insert(key, e.text.clone());
            }
            per_analysis.push(Some(edits));
        }

        // The readings that do not hold the declaration but read an edited file: the file may
        // mean something else there.
        let edited: HashSet<PathBuf> = by_place.keys().map(|k| k.0.clone()).collect();
        let before_overlay = Overlay::new(&self.buffers);
        let reader = Reader::new(&before_overlay, &self.settings);
        let mut extra: Vec<(Rc<Analysis>, Resolution)> = Vec::new();
        if pkg.local {
            // The same file read as a script, when the document is read as a module.
            let own = Spec::File(path.clone());
            if pkg.parts[0].0.spec != own
                && let Ok(a) = reader.analyze(&own, true)
            {
                extra.push((Rc::new(a), Resolution::default()));
            }
        }
        for pending in std::mem::take(&mut pkg.rest) {
            if pending.paths().iter().any(|f| edited.contains(f)) {
                let a = Rc::new(pending.check(&self.settings));
                let res = resolve(&a, None);
                extra.push((a, res));
            }
        }
        if !pkg.local {
            let all: Vec<(&Analysis, &Resolution)> = pkg
                .parts
                .iter()
                .chain(extra.iter())
                .map(|(a, r)| (&**a, r))
                .collect();
            let root = self.settings.root.as_deref();
            if let Some(message) = rename::conflicting_reading(&all, &what, &old_name, root) {
                return Err(message);
            }
        }

        // The edits by file, and the text of each file they touch (the same in every reading
        // that reads it).
        let mut by_file: BTreeMap<PathBuf, Vec<(u32, u32, String)>> = BTreeMap::new();
        for ((file_path, start, end), text) in &by_place {
            by_file
                .entry(file_path.clone())
                .or_default()
                .push((*start, *end, text.clone()));
        }
        // A rename edits the files of the open folder and no others, judged by canonical path
        // (a link inside the folder to a folder outside it does not make that one inside),
        // with one exception: the document the rename was asked in. A local variable or a
        // private item of an open file outside the folder is used in that file only, so
        // renaming it edits that file and nothing else. An item that other files can name is
        // refused earlier when it is declared outside, and a rename that would edit any other
        // file outside is refused here.
        if let Some(root) = &self.settings.root
            && let Some(outside) = by_file.keys().find(|f| !f.starts_with(root) && **f != path)
        {
            return Err(rename::outside_folder_message(
                &rename::shown(None, outside),
                &rename::shown(None, root),
            ));
        }
        let mut sources: HashMap<PathBuf, String> = HashMap::new();
        for file_path in by_file.keys() {
            let text = pkg
                .parts
                .iter()
                .chain(extra.iter())
                .find_map(|(a, _)| {
                    let f = a.file_of(file_path)?;
                    Some(a.sources.text(f).to_string())
                })
                .unwrap_or_default();
            sources.insert(file_path.clone(), text);
        }
        let mut buffers = self.buffers.clone();
        for (file_path, list) in &mut by_file {
            list.sort_by_key(|(start, ..)| std::cmp::Reverse(*start));
            let mut text = sources[file_path].clone();
            for (start, end, new) in list.iter() {
                text.replace_range(*start as usize..*end as usize, new);
            }
            buffers.insert(file_path.clone(), text);
        }

        // Read every reading again, with the edits applied.
        let overlay = Overlay::new(&buffers);
        let after_reader = Reader::new(&overlay, &self.settings);
        let root = self.settings.root.as_deref();
        let held = pkg.parts.len();
        let mut parts_after: Vec<Analysis> = Vec::new();
        for (before, _) in pkg.parts.iter().chain(extra.iter()) {
            let again = after_reader.analyze(&before.spec, true).map_err(|e| {
                format!(
                    "renaming to `{}` would leave `{}` unreadable ({}): not renamed",
                    p.new_name, e.path, e.message
                )
            })?;
            let mut was: Vec<PathBuf> = before.module_paths().collect();
            let mut now: Vec<PathBuf> = again.module_paths().collect();
            was.sort();
            now.sort();
            if was != now {
                return Err(
                    "the renamed program does not have the same modules: not renamed".into(),
                );
            }
            // The errors the rename adds: a name that is renamed may be in a message that
            // was there before, which is compared as the identifier it is.
            if let Some((file, message)) =
                rename::added_error(before, &again, &old_name, &p.new_name)
            {
                return Err(format!(
                    "renaming to `{}` would break the program ({}): {}: {message}",
                    p.new_name,
                    rename::describe(&before.spec, root),
                    rename::shown(root, &file),
                ));
            }
            parts_after.push(again);
        }
        // Every reading must resolve every name it resolved before to the same declaration,
        // whether or not it has errors elsewhere. (A local variable is used in one file, and
        // its other readings are not resolved.)
        let compared = if pkg.local { held } else { parts_after.len() };
        for (i, ((before, res), after)) in pkg
            .parts
            .iter()
            .chain(extra.iter())
            .zip(parts_after.iter())
            .take(compared)
            .enumerate()
        {
            // A program that did not load was not checked: nothing is resolved in it, so there
            // is nothing to compare (a stated limit).
            if before.program.has_errors() {
                continue;
            }
            // The edits that name the declaration in this reading, then the edits other
            // readings make in the files it reads (nothing in it is the name itself there).
            let mut edits = per_analysis
                .get_mut(i)
                .and_then(Option::take)
                .unwrap_or_default();
            let have: HashSet<colibri_syntax::Span> = edits.iter().map(|e| e.span).collect();
            edits.extend(
                edits_in(before, &by_file)
                    .into_iter()
                    .filter(|e| !have.contains(&e.span)),
            );
            let after_res = resolve(after, None);
            let shift = rename::Shift::new(before, &edits);
            rename::compare(
                (before, res),
                (after, &after_res),
                &shift,
                self.settings.root.as_deref(),
            )?;
        }

        // `WorkspaceEdit::changes` is keyed by `Uri`, which lsp-types fixes.
        #[allow(clippy::mutable_key_type)]
        let mut changes: HashMap<Uri, Vec<TextEdit>> = HashMap::new();
        for (file_path, list) in &by_file {
            let Some(uri) = self.uri_of(file_path) else {
                return Err(rename::no_address_message(&rename::shown(
                    self.settings.root.as_deref(),
                    file_path,
                )));
            };
            let index = LineIndex::new(&sources[file_path]);
            let out = changes.entry(uri).or_default();
            for (start, end, text) in list.iter().rev() {
                out.push(TextEdit::new(
                    index.range(*start as usize, *end as usize),
                    text.clone(),
                ));
            }
        }
        Ok(Some(WorkspaceEdit {
            changes: Some(changes),
            ..WorkspaceEdit::default()
        }))
    }

    /// The fact at a position of a document: its analysis, the file, the fact and the text.
    fn fact_at(
        &self,
        p: &lsp_types::TextDocumentPositionParams,
    ) -> Option<(
        Rc<Analysis>,
        colibri_syntax::FileId,
        colibri_check::insight::Fact,
    )> {
        let path = key(&p.text_document.uri);
        if !is_source(&path) {
            return None;
        }
        let analysis = self.latest.get(&path)?.clone();
        let file = analysis.file_of(&path)?;
        let text = analysis.sources.text(file);
        let offset = LineIndex::new(text).offset(p.position);
        let (start, word) = word_at(text, offset);
        let fact = analysis.insight.at(file, start, word)?.clone();
        Some((analysis, file, fact))
    }

    /// Hover on a type name the checker has no fact for (one written in an annotation, such
    /// as `kind: fs.Error`): what the name denotes.
    fn type_hover(&self, p: &lsp_types::TextDocumentPositionParams) -> Option<Hover> {
        let path = key(&p.text_document.uri);
        if !is_source(&path) {
            return None;
        }
        let analysis = self.latest.get(&path)?.clone();
        let file = analysis.file_of(&path)?;
        let text = analysis.sources.text(file);
        let offset = LineIndex::new(text).offset(p.position) as u32;
        let res = resolve(&analysis, None);
        let occ = &res.occs[rename::occ_at(&res, file, offset)?];
        let shown = match &occ.target {
            Target::Def(d) => {
                let info = res.defs.get(d)?;
                if info.kind != DefKind::Type {
                    return None;
                }
                let items = analysis.program.files.get(info.module)?.items.iter();
                let keyword = items.filter_map(|top| match top {
                    colibri_syntax::ast::TopLevel::Item(i) => Some(&i.kind),
                    colibri_syntax::ast::TopLevel::Stmt(_) => None,
                });
                let mut word = "type";
                for kind in keyword {
                    use colibri_syntax::ast::{ItemKind, StructKind};
                    match kind {
                        ItemKind::Struct(s) if s.name.span == *d => {
                            word = if s.kind == StructKind::Class {
                                "class"
                            } else {
                                "struct"
                            };
                        }
                        ItemKind::Enum(e) if e.name.span == *d => word = "enum",
                        ItemKind::Interface(i) if i.name.span == *d => word = "interface",
                        _ => {}
                    }
                }
                format!("{word} {}", info.name)
            }
            Target::External(name) => {
                let name = name.strip_prefix("std.").unwrap_or(name);
                let last = name.rsplit('.').next()?;
                if !last.chars().next()?.is_uppercase() {
                    return None;
                }
                name.to_string()
            }
        };
        let span = occ.span;
        let index = LineIndex::new(text);
        Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: format!("```colibri\n{shown}\n```"),
            }),
            range: Some(index.range(span.start as usize, span.end as usize)),
        })
    }

    fn hover(&self, p: lsp_types::HoverParams) -> Option<Hover> {
        let Some((analysis, file, fact)) = self.fact_at(&p.text_document_position_params) else {
            return self.type_hover(&p.text_document_position_params);
        };
        let index = LineIndex::new(analysis.sources.text(file));
        let mut value = format!("```colibri\n{}\n```", fact.text);
        if let Some(message) = fact.def.and_then(|d| analysis.insight.deprecation(d)) {
            value.push_str("\n\n**Deprecated**");
            if !message.is_empty() {
                value.push_str(&format!(": {message}"));
            }
        }
        Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value,
            }),
            range: Some(index.range(fact.span.start as usize, fact.span.end as usize)),
        })
    }

    fn definition(&self, p: lsp_types::GotoDefinitionParams) -> Option<GotoDefinitionResponse> {
        let (analysis, _, fact) = self.fact_at(&p.text_document_position_params)?;
        let def = fact.def?;
        if !analysis.sources.contains(def.file) {
            return None;
        }
        let path = analysis.path_of(def.file);
        let index = LineIndex::new(analysis.sources.text(def.file));
        Some(GotoDefinitionResponse::Scalar(Location::new(
            self.uri_of(&path)?,
            index.range(def.start as usize, def.end as usize),
        )))
    }
}

/// The edits of `by_file` (by canonical path: start, end and text) in the files of
/// `analysis`, as edits of that reading.
fn edits_in(
    analysis: &Analysis,
    by_file: &BTreeMap<PathBuf, Vec<(u32, u32, String)>>,
) -> Vec<rename::Edit> {
    let mut out = Vec::new();
    for (file, path) in analysis.files() {
        for (start, end, text) in by_file.get(&path).into_iter().flatten() {
            out.push(rename::Edit {
                span: colibri_syntax::Span::new(file, *start, *end),
                text: text.clone(),
                pieces: Vec::new(),
            });
        }
    }
    out
}

/// The readings of a package, what each resolves to, and the name at a cursor.
struct Package {
    /// The readings that can hold the name at the cursor, checked; the first is the
    /// document's own.
    parts: Vec<(Rc<Analysis>, Resolution)>,
    /// The other readings, loaded but not checked: none holds the declaration.
    rest: Vec<Pending>,
    /// The occurrence of the name at the cursor in the first analysis.
    at: Option<usize>,
    /// What that name stands for.
    what: Option<rename::Meaning>,
    /// Only the first analysis is used: the name is a local variable.
    local: bool,
    /// See [`crate::analysis::Coverage::editable`].
    editable: Vec<PathBuf>,
    /// See [`crate::analysis::Coverage::incomplete`].
    incomplete: Option<String>,
}

impl Package {
    /// The package as far as the analysis of the document goes.
    fn new(
        analyses: Vec<Rc<Analysis>>,
        path: &Path,
        position: lsp_types::Position,
    ) -> Option<Package> {
        let parts: Vec<(Rc<Analysis>, Resolution)> = analyses
            .into_iter()
            .map(|a| {
                let res = resolve(&a, None);
                (a, res)
            })
            .collect();
        let (first, res) = parts.first()?;
        let file = first.file_of(path)?;
        let offset = LineIndex::new(first.sources.text(file)).offset(position) as u32;
        let at = rename::occ_at(res, file, offset);
        let what = at.map(|i| rename::meaning(first, &res.occs[i].target));
        let local = at.is_some_and(|i| {
            matches!(&res.occs[i].target, Target::Def(d)
                if res.defs.get(d).is_some_and(|info| info.kind == DefKind::Local))
        });
        Some(Package {
            parts,
            rest: Vec::new(),
            at,
            what,
            local,
            editable: Vec::new(),
            incomplete: None,
        })
    }

    /// Adds the rest of the readings: those that hold the declaration are checked, the others
    /// wait until a rename knows which files it edits.
    fn extend(&mut self, coverage: crate::analysis::Coverage, settings: &Settings) {
        let decl = match &self.what {
            Some(rename::Meaning::Def(path, ..)) => Some(path.clone()),
            _ => None,
        };
        for pending in coverage.pending {
            // A name from the prelude or a dependency has no file: any reading may use it.
            if decl.as_ref().is_none_or(|d| pending.holds(d)) {
                let a = Rc::new(pending.check(settings));
                let res = resolve(&a, None);
                self.parts.push((a, res));
            } else {
                self.rest.push(pending);
            }
        }
        self.editable = coverage.editable;
        self.incomplete = coverage.incomplete;
    }

    fn is_local(&self) -> bool {
        self.local
    }

    /// Refuses a rename while a file the rename reads does not load: after a syntax error the
    /// parser drops whole statements, so the uses inside them are not known and the rename
    /// would miss them. `doc` is the document the rename starts from.
    fn refuse_if_broken(&self, doc: &Path) -> Result<(), String> {
        let only = self.local.then_some(doc);
        for (analysis, _) in &self.parts {
            refuse_if_broken(&analysis.sources, &analysis.program, only)?;
        }
        for pending in &self.rest {
            let (sources, loaded) = pending.program();
            refuse_if_broken(sources, loaded, only)?;
        }
        Ok(())
    }

    /// Refuses a rename while an item that a false `@when` left out of the program mentions the
    /// name: the program does not read that item, so its uses would not be renamed and the
    /// other configuration would break. A local variable cannot be used by such an item.
    fn refuse_if_left_out(&self, info: &DefInfo, root: Option<&Path>) -> Result<(), String> {
        if self.local {
            return Ok(());
        }
        for (analysis, _) in &self.parts {
            refuse_if_left_out(&analysis.sources, &analysis.program, &info.name, root)?;
        }
        for pending in &self.rest {
            let (sources, loaded) = pending.program();
            refuse_if_left_out(sources, loaded, &info.name, root)?;
        }
        Ok(())
    }

    /// Refuses to rename something other files can name when not every file that could was
    /// found.
    fn refuse_if_incomplete(&self, info: &DefInfo) -> Result<(), String> {
        match &self.incomplete {
            Some(why) if info.visible => Err(format!(
                "cannot rename `{}`, which other files may use: {why}",
                info.name
            )),
            _ => Ok(()),
        }
    }
}

/// Whether a rename is refused because part of a program was left out of it: a syntax error
/// (the parser drops whole statements, so the uses inside them are unknown), an `import`
/// that matches several modules, or a file that cannot be read. Other errors do not matter:
/// an `import` of a module that does not exist, a dependency that is not in the cache, a bad
/// manifest, or a type error leaves nothing of the package out. With `only` (a local
/// variable), only a syntax error in that file counts, since no other file can use the name.
fn refuse_if_broken(
    sources: &SourceMap,
    program: &Loaded,
    only: Option<&Path>,
) -> Result<(), String> {
    for d in &program.dropped {
        if !sources.contains(d.span.file) {
            continue;
        }
        let path = canon(Path::new(sources.name(d.span.file)));
        let in_dependency = program
            .files
            .iter()
            .position(|f| f.span.file == d.span.file)
            .and_then(|m| program.metas.get(m))
            .is_some_and(|m| m.dependency);
        if in_dependency {
            continue;
        }
        if let Some(doc) = only
            && (path != doc || d.kind != DroppedKind::Syntax)
        {
            continue;
        }
        let name = path.file_name().map_or_else(
            || path.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        let line = LineIndex::new(sources.text(d.span.file))
            .range(d.span.start as usize, d.span.end as usize)
            .start
            .line
            + 1;
        let what = match d.kind {
            DroppedKind::Syntax => "has an error that stops the program from loading",
            DroppedKind::Ambiguous => "has an import that matches more than one module",
            DroppedKind::Unreadable => "imports a file that cannot be read",
        };
        return Err(format!(
            "cannot rename while `{name}` (line {line}) {what} ({}): fix it first, or the rename could miss names",
            d.message
        ));
    }
    Ok(())
}

/// Whether `name` is written as a whole word (not part of a longer name) in `text`.
fn mentions_word(text: &str, name: &str) -> bool {
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    text.match_indices(name).any(|(at, _)| {
        !text[..at].chars().next_back().is_some_and(is_word)
            && !text[at + name.len()..].chars().next().is_some_and(is_word)
    })
}

/// Whether a rename of `name` is refused because an item that a false `@when` left out of a
/// module of the package mentions it.
fn refuse_if_left_out(
    sources: &SourceMap,
    program: &Loaded,
    name: &str,
    root: Option<&Path>,
) -> Result<(), String> {
    for (module, meta) in program.metas.iter().enumerate() {
        if meta.dependency {
            continue;
        }
        let Some(file) = program.files.get(module) else {
            continue;
        };
        for left in &meta.excluded {
            let span = left.span;
            if span.file != file.span.file || !sources.contains(span.file) {
                continue;
            }
            let Some(text) = sources
                .text(span.file)
                .get(span.start as usize..span.end as usize)
            else {
                continue;
            };
            if !mentions_word(text, name) {
                continue;
            }
            let path = canon(Path::new(sources.name(span.file)));
            // Relative to the open folder with `/`: a workspace can have two `main.colibri`.
            let shown = rename::shown(root, &path);
            let line = LineIndex::new(sources.text(span.file))
                .range(span.start as usize, span.end as usize)
                .start
                .line
                + 1;
            return Err(format!(
                "cannot rename `{name}` while `{shown}` (line {line}) has code left out here by `@when({})` that mentions it: the rename would not reach it and would break the other configuration",
                left.cond
            ));
        }
    }
    Ok(())
}

/// The identifier around a byte offset: where it starts (or `offset` if there is none) and
/// the word.
fn word_at(text: &str, offset: usize) -> (u32, &str) {
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let mut offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    let start = text[..offset]
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_word(*c))
        .last()
        .map_or(offset, |(i, _)| i);
    let end = text[offset..]
        .char_indices()
        .find(|(_, c)| !is_word(*c))
        .map_or(text.len(), |(i, _)| offset + i);
    (start as u32, &text[start..end])
}

/// The interface files named by the client's initialization options
/// (`{"interfaces": ["engine.cbi"]}`, or the same under a `colibri` key), relative paths taken
/// from the workspace folder.
/// The key of a document: its canonical path.
fn key(uri: &Uri) -> PathBuf {
    canon(&uri_to_path(uri))
}

fn interface_paths(options: Option<&Value>, root: Option<&Path>) -> Vec<PathBuf> {
    let Some(options) = options else {
        return Vec::new();
    };
    let list = options
        .get("interfaces")
        .or_else(|| options.get("colibri").and_then(|c| c.get("interfaces")))
        .and_then(Value::as_array);
    list.into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|p| {
            let p = Path::new(p);
            match root {
                Some(root) if p.is_relative() => normalize(&root.join(p)),
                _ => normalize(p),
            }
        })
        .collect()
}

/// Whether `message` is the checker's warning about a use of a `@deprecated` item.
fn is_deprecation_warning(message: &str) -> bool {
    message.starts_with('`')
        && (message.ends_with("` is deprecated") || message.contains("` is deprecated: "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_word_under_the_cursor() {
        let text = "let ab_1 = é+x";
        assert_eq!(word_at(text, 5), (4, "ab_1"));
        assert_eq!(word_at(text, 8), (4, "ab_1"));
        assert_eq!(word_at(text, 9), (9, ""));
        let e = text.find('é').unwrap();
        assert_eq!(word_at(text, e), (e as u32, "é"));
        assert_eq!(word_at(text, text.len()), (text.len() as u32 - 1, "x"));
    }

    #[test]
    fn interface_options() {
        let v = serde_json::json!({"interfaces": ["a.cbi", "/abs/b.cbi"]});
        let paths = interface_paths(Some(&v), Some(Path::new("/w")));
        assert_eq!(paths, [Path::new("/w/a.cbi"), Path::new("/abs/b.cbi")]);
        let v = serde_json::json!({"colibri": {"interfaces": ["c.cbi"]}});
        assert_eq!(interface_paths(Some(&v), None), [Path::new("c.cbi")]);
        assert!(interface_paths(None, None).is_empty());
    }
}
