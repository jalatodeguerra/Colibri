//! The message loop and the handlers.

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use colibri_check::DEFAULT_STACK_BUDGET;
use colibri_check::package::normalize;
use colibri_syntax::{Severity, format_source};
use lsp_server::{Connection, ErrorCode, Message, Notification, Request, RequestId, Response};
use lsp_types::notification::{
    DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument, DidSaveTextDocument,
    LogMessage, Notification as _, PublishDiagnostics,
};
use lsp_types::request::{Formatting, GotoDefinition, HoverRequest, Request as _};
use lsp_types::{
    Diagnostic, DiagnosticRelatedInformation, DiagnosticSeverity, GotoDefinitionResponse, Hover,
    HoverContents, HoverProviderCapability, InitializeParams, Location, LogMessageParams,
    MarkupContent, MarkupKind, MessageType, OneOf, PublishDiagnosticsParams, ServerCapabilities,
    ServerInfo, TextDocumentSyncCapability, TextDocumentSyncKind, TextEdit, Uri,
};
use serde_json::Value;

use crate::analysis::{Analysis, Overlay, Settings, analyze};
use crate::convert::{LineIndex, path_to_uri, uri_to_path};

type BoxError = Box<dyn Error + Send + Sync>;

/// Settings of the server that the client does not send.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// How much native stack checking may use (see `colibri_check::check_with_stack_budget`).
    pub stack_budget: usize,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            stack_budget: DEFAULT_STACK_BUDGET,
        }
    }
}

/// Serves the protocol on stdin and stdout until the client asks to exit.
pub fn run_stdio(config: Config) -> Result<(), BoxError> {
    let (connection, io_threads) = Connection::stdio();
    serve(connection, config)?;
    io_threads.join()?;
    Ok(())
}

/// Serves the protocol on `connection` until the client asks to exit.
pub fn serve(connection: Connection, config: Config) -> Result<(), BoxError> {
    let capabilities = ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
        document_formatting_provider: Some(OneOf::Left(true)),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        definition_provider: Some(OneOf::Left(true)),
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
    for message in &connection.receiver {
        match message {
            Message::Request(req) => {
                if connection.handle_shutdown(&req)? {
                    return Ok(());
                }
                server.request(req);
            }
            Message::Notification(note) => server.notification(note),
            Message::Response(_) => {}
        }
    }
    Ok(())
}

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
    /// The files each open document's package published diagnostics for.
    published: HashMap<PathBuf, HashSet<PathBuf>>,
    /// Interface files already reported as unreadable, so the message is sent once.
    reported: HashSet<PathBuf>,
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
        Server {
            connection,
            settings: Settings {
                stack_budget: config.stack_budget,
                interfaces,
                root,
            },
            docs: HashMap::new(),
            buffers: HashMap::new(),
            latest: HashMap::new(),
            published: HashMap::new(),
            reported: HashSet::new(),
        }
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
                    self.set_text(p.text_document.uri, p.text_document.text);
                }
            }
            DidChangeTextDocument::METHOD => {
                if let Ok(p) =
                    serde_json::from_value::<lsp_types::DidChangeTextDocumentParams>(note.params)
                {
                    // Full synchronisation: the last change is the whole text.
                    if let Some(change) = p.content_changes.into_iter().last() {
                        self.set_text(p.text_document.uri, change.text);
                    }
                }
            }
            DidSaveTextDocument::METHOD => {
                if let Ok(p) =
                    serde_json::from_value::<lsp_types::DidSaveTextDocumentParams>(note.params)
                {
                    // The buffer is what was saved; a save can still change what imports read.
                    if let Some(doc) = self
                        .docs
                        .get(&normalize(&uri_to_path(&p.text_document.uri)))
                    {
                        let (uri, text) = (doc.uri.clone(), doc.text.clone());
                        self.set_text(uri, text);
                    }
                }
            }
            DidCloseTextDocument::METHOD => {
                if let Ok(p) =
                    serde_json::from_value::<lsp_types::DidCloseTextDocumentParams>(note.params)
                {
                    self.close(&normalize(&uri_to_path(&p.text_document.uri)));
                }
            }
            _ => {}
        }
    }

    fn set_text(&mut self, uri: Uri, text: String) {
        let path = normalize(&uri_to_path(&uri));
        self.buffers.insert(path.clone(), text.clone());
        self.docs.insert(path.clone(), Doc { uri, text });
        self.check(&path);
    }

    fn close(&mut self, path: &Path) {
        self.docs.remove(path);
        self.buffers.remove(path);
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
                self.check(&other);
            }
        }
    }

    /// Checks the package of `doc` and publishes what it found.
    fn check(&mut self, doc: &Path) {
        let overlay = Overlay {
            buffers: &self.buffers,
        };
        let (analysis, hosts) = analyze(doc, &overlay, &self.settings);
        for (path, message) in hosts.problems {
            if self.reported.insert(path.clone()) {
                self.log(format!(
                    "colibri: cannot use the interface file {}: {message}",
                    path.display()
                ));
            }
        }
        let analysis = Rc::new(analysis);
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
    }

    fn uri_of(&self, path: &Path) -> Option<Uri> {
        match self.docs.get(path) {
            Some(doc) => Some(doc.uri.clone()),
            None => path_to_uri(path),
        }
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
            let path = normalize(Path::new(sources.name(span.file)));
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
        let result: Result<Value, String> = match method.as_str() {
            Formatting::METHOD => self.reply(params, Self::formatting),
            HoverRequest::METHOD => self.reply(params, Self::hover),
            GotoDefinition::METHOD => self.reply(params, Self::definition),
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
            Err(message) => self.respond_err(id, ErrorCode::InvalidParams, message),
        }
    }

    fn respond_err(&self, id: RequestId, code: ErrorCode, message: String) {
        self.send(Response::new_err(id, code as i32, message));
    }

    fn reply<P: serde::de::DeserializeOwned, R: serde::Serialize>(
        &self,
        params: Value,
        f: fn(&Self, P) -> R,
    ) -> Result<Value, String> {
        let params = serde_json::from_value(params).map_err(|e| e.to_string())?;
        serde_json::to_value(f(self, params)).map_err(|e| e.to_string())
    }

    fn formatting(&self, p: lsp_types::DocumentFormattingParams) -> Option<Vec<TextEdit>> {
        let path = normalize(&uri_to_path(&p.text_document.uri));
        let doc = self.docs.get(&path)?;
        // A file with syntax errors is left alone.
        let formatted = format_source(&path.to_string_lossy(), &doc.text).ok()?;
        if formatted == doc.text {
            return Some(Vec::new());
        }
        let range = LineIndex::new(&doc.text).whole();
        Some(vec![TextEdit::new(range, formatted)])
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
        let path = normalize(&uri_to_path(&p.text_document.uri));
        let analysis = self.latest.get(&path)?.clone();
        let file = analysis.file_of(&path)?;
        let text = analysis.sources.text(file);
        let offset = LineIndex::new(text).offset(p.position);
        let (start, word) = word_at(text, offset);
        let fact = analysis.insight.at(file, start, word)?.clone();
        Some((analysis, file, fact))
    }

    fn hover(&self, p: lsp_types::HoverParams) -> Option<Hover> {
        let (analysis, file, fact) = self.fact_at(&p.text_document_position_params)?;
        let index = LineIndex::new(analysis.sources.text(file));
        Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: format!("```colibri\n{}\n```", fact.text),
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
        let path = normalize(Path::new(analysis.sources.name(def.file)));
        let index = LineIndex::new(analysis.sources.text(def.file));
        Some(GotoDefinitionResponse::Scalar(Location::new(
            self.uri_of(&path)?,
            index.range(def.start as usize, def.end as usize),
        )))
    }
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
