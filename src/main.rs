use std::convert::From;
use std::env;
use std::fmt::Display;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::RwLock;
use tower_lsp::jsonrpc::{ErrorCode, Result};
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};
use tree_sitter::{Language, Node};

mod beancount;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("I/O error")]
    IoError(#[from] std::io::Error),

    #[error("UTF8 conversion error")]
    Utf8Error(#[from] std::str::Utf8Error),

    #[error("ParseInt error")]
    ParseIntError(#[from] std::num::ParseIntError),

    #[error("Language error")]
    LanguageError(#[from] tree_sitter::LanguageError),

    #[error("Tree parse error")]
    TreeParseError,

    #[error("Trie is empty")]
    TrieEmpty,

    #[error("Cannot convert URI to file path")]
    UriToPathConversion,

    #[error("Unexpected format error")]
    UnexpectedFormat,

    #[error("Invalid state")]
    InvalidState,
}

impl From<Error> for tower_lsp::jsonrpc::Error {
    fn from(error: Error) -> Self {
        Self {
            code: ErrorCode::ServerError(0),
            message: error.to_string(),
            data: None,
        }
    }
}

#[derive(Default)]
struct State {
    data: beancount::Data,
}

fn node_text<'a>(node: &'a Node, text: &'a str) -> Result<&'a str> {
    Ok(node.utf8_text(text.as_bytes()).map_err(Error::from)?)
}

fn item_from_str<T: Into<String>>(label: T) -> CompletionItem {
    CompletionItem::new_simple(label.into(), "".to_string())
}

impl State {
    fn complete_account(&self) -> Result<Option<CompletionResponse>> {
        Ok(Some(CompletionResponse::Array(
            self.data.accounts.iter().map(item_from_str).collect(),
        )))
    }

    fn complete_currency(&self) -> Result<Option<CompletionResponse>> {
        Ok(Some(CompletionResponse::Array(
            self.data.currencies.iter().map(item_from_str).collect(),
        )))
    }

    fn handle_identifier(&self, node: &Node) -> Result<Option<CompletionResponse>> {
        // This happens for initial completions, i.e. if a character has not triggered
        // yet. This means this is likely one of the top-level accounts or a payee.
        let identifier = node_text(node, &self.data.text)?;

        for account in ["Expenses", "Assets", "Liabilities", "Equity", "Revenue"] {
            // Yes, for some stupid reason, the first character is matched as an ERROR
            // and the identifier starts with the second character ...
            if account[1..].starts_with(identifier) {
                return Ok(Some(CompletionResponse::Array(vec![item_from_str(
                    account,
                )])));
            }
        }

        Ok(None)
    }

    fn handle_error(&self, node: &Node) -> Result<Option<CompletionResponse>> {
        let identifier = node_text(node, &self.data.text)?;

        // Probably, hopefully starts with " and ends with some weird character yet to be
        // identified.
        let prefix = &identifier[1..].trim_end();

        let candidates = self
            .data
            .payees
            .iter()
            .filter(|p| p.starts_with(prefix))
            .map(item_from_str)
            .collect::<Vec<_>>();

        if candidates.is_empty() {
            Ok(None)
        } else {
            Ok(Some(CompletionResponse::Array(candidates)))
        }
    }

    fn handle_node(&self, node: &Node) -> Result<Option<CompletionResponse>> {
        match node.kind() {
            "currency" => self.complete_currency(),
            "identifier" => self.handle_identifier(node),
            "account" => self.complete_account(),
            "ERROR" => self.handle_error(node),
            _ => Ok(None),
        }
    }
}

struct Backend {
    client: Option<Client>,
    check_cmd: Option<PathBuf>,
    check_re: regex::Regex,
    state: Arc<RwLock<State>>,
    language: Language,
}

impl Backend {
    fn new(client: Client) -> Self {
        let check_cmd = env::var_os("PATH").and_then(|paths| {
            env::split_paths(&paths).find_map(|p| {
                let full_path = p.join("bean-check");

                if full_path.is_file() {
                    Some(full_path)
                } else {
                    None
                }
            })
        });

        Self {
            client: Some(client),
            check_cmd,
            check_re: regex::Regex::new(r"^[^:]+:(\d+):\s*(.*)$").expect("failed to compile regex"),
            language: tree_sitter_beancount::language(),
            state: Arc::new(RwLock::new(State::default())),
        }
    }

    /// Load ledger to search trie and lines.
    async fn load_ledgers(&self, uri: &Url) -> Result<()> {
        let mut state = self.state.write().await;
        state.data = beancount::Data::new(uri)?;
        Ok(())
    }

    async fn log_message<M: Display>(&self, typ: MessageType, message: M) {
        if let Some(client) = &self.client {
            client.log_message(typ, message).await;
        }
    }

    async fn check(&self, uri: Url) -> Result<()> {
        let client = self.client.as_ref().ok_or(Error::InvalidState)?;

        let check_cmd = self.check_cmd.as_ref().ok_or(Error::InvalidState)?;

        let output = Command::new(check_cmd)
            .arg(uri.path())
            .output()
            .await
            .map_err(Error::from)?;

        let diags = if !output.status.success() {
            let output = std::str::from_utf8(&output.stderr).map_err(Error::from)?;

            output
                .lines()
                .filter_map(|line| self.check_re.captures(line))
                .map(|caps| {
                    let line = caps[1]
                        .parse::<u32>()
                        .map_err(Error::ParseIntError)?
                        .saturating_sub(1);

                    let position = Position { line, character: 0 };

                    Ok(Diagnostic {
                        range: Range {
                            start: position,
                            end: position,
                        },
                        message: caps[2].trim().to_string(),
                        ..Diagnostic::default()
                    })
                })
                .collect::<Result<Vec<_>>>()?
        } else {
            vec![]
        };

        client.publish_diagnostics(uri, diags, None).await;
        Ok(())
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            server_info: Some(ServerInfo {
                name: "beancount-language-server".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
            capabilities: ServerCapabilities {
                // TODO: incremental is probably smarter
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                completion_provider: Some(CompletionOptions {
                    resolve_provider: Some(false),
                    trigger_characters: Some(vec![":".to_string()]),
                    work_done_progress_options: Default::default(),
                    all_commit_characters: None,
                }),
                definition_provider: Some(OneOf::Left(true)),
                document_formatting_provider: Some(OneOf::Left(true)),
                ..Default::default()
            },
        })
    }

    async fn initialized(&self, _: InitializedParams) {}

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        if let Err(err) = self.load_ledgers(&params.text_document.uri).await {
            self.log_message(MessageType::ERROR, err.to_string()).await;
        }

        if let Err(err) = self.check(params.text_document.uri).await {
            self.log_message(MessageType::ERROR, err.to_string()).await;
        }
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let mut state = self.state.write().await;
        state.data.text = params.content_changes[0].text.clone();
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        if let Err(err) = self.check(params.text_document.uri).await {
            self.log_message(MessageType::ERROR, err.to_string()).await;
        }
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let state = self.state.read().await;

        let mut parser = tree_sitter::Parser::new();
        parser.set_language(self.language).map_err(Error::from)?;

        let tree = parser
            .parse(&state.data.text, None)
            .ok_or(Error::TreeParseError)?;

        let line = params.text_document_position.position.line as usize;
        let char = params.text_document_position.position.character as usize;

        let start = tree_sitter::Point {
            row: line,
            column: if char == 0 { char } else { char - 1 },
        };

        let end = tree_sitter::Point {
            row: line,
            column: char,
        };

        let is_character_triggered = params
            .context
            .and_then(|c| c.trigger_character)
            .and_then(|c| if c == ":" { Some(()) } else { None })
            .is_some();

        let node = tree
            .root_node()
            .named_descendant_for_point_range(start, end);

        match node {
            Some(node) => {
                if is_character_triggered {
                    state.complete_account()
                } else {
                    state.handle_node(&node)
                }
            }
            None => Ok(None),
        }
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let state = self.state.read().await;

        let mut parser = tree_sitter::Parser::new();
        parser.set_language(self.language).map_err(Error::from)?;

        let tree = parser
            .parse(&state.data.text, None)
            .ok_or(Error::TreeParseError)?;

        let line = params.text_document_position_params.position.line as usize;
        let char = params.text_document_position_params.position.character as usize;

        let point = tree_sitter::Point {
            row: line,
            column: char,
        };

        if let Some(node) = tree
            .root_node()
            .named_descendant_for_point_range(point, point)
        {
            if node.kind() == "currency" {
                return Ok(state
                    .data
                    .commodities
                    .get(node_text(&node, &state.data.text)?)
                    .map(|loc| GotoDefinitionResponse::Array(vec![loc.clone()])));
            }
        }

        Ok(None)
    }

    async fn formatting(&self, params: DocumentFormattingParams) -> Result<Option<Vec<TextEdit>>> {
        // Lets use brute force and delete everything and add the newly formatted stuff back.
        let state = self.state.read().await;
        let formatted = beancount::reformat(&params.text_document.uri)?.map(|formatted| {
            vec![TextEdit {
                range: Range {
                    start: Position::default(),
                    end: Position {
                        line: state.data.text.matches('\n').count() as u32,
                        character: 0,
                    },
                },
                new_text: formatted,
            }]
        });
        Ok(formatted)
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}

#[tokio::main]
async fn main() {
    let (stdin, stdout) = (tokio::io::stdin(), tokio::io::stdout());
    let (service, socket) = LspService::new(Backend::new);

    Server::new(stdin, stdout, socket).serve(service).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::Path;

    impl Backend {
        fn new_without_client() -> Self {
            Self {
                client: None,
                check_cmd: None,
                check_re: regex::Regex::new(r"").expect("unable to build empty regex"),
                language: tree_sitter_beancount::language(),
                state: Arc::new(RwLock::new(State::default())),
            }
        }
    }

    fn url_from_file_path<P: AsRef<Path>>(path: P) -> std::result::Result<Url, Error> {
        Ok(Url::from_file_path(path).map_err(|_| Error::UriToPathConversion)?)
    }

    #[ignore]
    #[tokio::test]
    async fn complete_root_account() -> std::result::Result<(), Error> {
        let mut file = tempfile::NamedTempFile::new()?;

        write!(
            file.as_file_mut(),
            r#"2021-07-11 "foo" "bar"
  Expe
        "#
        )?;

        let backend = Backend::new_without_client();
        let uri = url_from_file_path(file.path())?;
        backend.load_ledgers(&uri).await.unwrap();

        let params = CompletionParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: Position {
                    line: 1,
                    character: 5,
                },
            },
            context: Some(CompletionContext {
                trigger_kind: CompletionTriggerKind::INVOKED,
                trigger_character: None,
            }),
            work_done_progress_params: WorkDoneProgressParams {
                work_done_token: None,
            },
            partial_result_params: PartialResultParams {
                partial_result_token: None,
            },
        };

        let result = backend.completion(params).await.unwrap().unwrap();

        match result {
            CompletionResponse::Array(items) => {
                assert_eq!(items.len(), 1);
                assert_eq!(items[0].label, "Expenses");
            }
            _ => assert!(false),
        };

        Ok(())
    }

    #[tokio::test]
    async fn complete_payee() -> std::result::Result<(), Error> {
        let mut file = tempfile::NamedTempFile::new()?;

        write!(
            file.as_file_mut(),
            r#"2021-07-11 "foo" "bar"
  Expenses:Foo:Bar
2021-07-11 "faa" "bar"
  Expenses:Foo:Bar
2021-07-11 "gaa" "bar"
  Expenses:Foo:Bar
2021-07-11 "f
        "#
        )?;

        let backend = Backend::new_without_client();
        let uri = url_from_file_path(file.path())?;
        backend.load_ledgers(&uri).await.unwrap();

        let params = CompletionParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: Position {
                    line: 6,
                    character: 12,
                },
            },
            context: Some(CompletionContext {
                trigger_kind: CompletionTriggerKind::INVOKED,
                trigger_character: None,
            }),
            work_done_progress_params: WorkDoneProgressParams {
                work_done_token: None,
            },
            partial_result_params: PartialResultParams {
                partial_result_token: None,
            },
        };

        let result = backend.completion(params).await.unwrap().unwrap();

        match result {
            CompletionResponse::Array(items) => {
                assert_eq!(items.len(), 2);

                for item in items {
                    assert!(item.label == "foo" || item.label == "faa");
                }
            }
            _ => assert!(false),
        };

        Ok(())
    }
}
