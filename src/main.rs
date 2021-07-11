use std::collections::HashMap;
use std::convert::From;
use std::fmt::Display;
use std::sync::Arc;
use tokio::sync::RwLock;
use tower_lsp::jsonrpc::{ErrorCode, Result};
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};
use tree_sitter::{Language, Node};
use trie_rs::Trie;

mod beancount;

#[derive(thiserror::Error, Debug)]
enum Error {
    #[error("UTF8 conversion error")]
    Utf8Error(#[from] std::str::Utf8Error),

    #[error("Language error")]
    LanguageError(#[from] tree_sitter::LanguageError),

    #[error("Trie is empty")]
    TrieEmpty,
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

struct State {
    text: String,
    commodities: HashMap<String, Location>,
    account_trie: Option<Trie<String>>,
    currency_trie: Option<Trie<char>>,
}

fn node_text<'a>(node: &'a Node, text: &'a str) -> Result<&'a str> {
    Ok(node.utf8_text(text.as_bytes()).map_err(Error::from)?)
}

fn account_sequence_from(node: &Node, text: &str) -> Result<Vec<String>> {
    let account = node_text(node, text)?.to_string();

    Ok(account
        .split(':')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect())
}

fn completion_response_from(
    sequence: &[String],
    trie: &Option<Trie<String>>,
) -> Result<CompletionResponse> {
    match trie {
        Some(trie) => {
            let result = trie.predictive_search(&sequence);
            let prefix_length = sequence.len();

            Ok(CompletionResponse::Array(
                result
                    .iter()
                    .map(|seq| {
                        CompletionItem::new_simple(seq[prefix_length..].join(":"), "".to_string())
                    })
                    .collect(),
            ))
        }
        None => Err(Error::TrieEmpty.into()),
    }
}

impl State {
    fn handle_character_triggered(&self, node: &Node) -> Result<Option<CompletionResponse>> {
        let sequence = account_sequence_from(node, &self.text)?;

        if sequence.is_empty() {
            return Ok(None);
        }

        Ok(Some(completion_response_from(
            &sequence,
            &self.account_trie,
        )?))
    }

    fn handle_account(&self, node: &Node) -> Result<Option<CompletionResponse>> {
        let sequence = account_sequence_from(node, &self.text)?;

        if sequence.len() < 2 {
            return Ok(None);
        }

        // Drop the trailing incomplete account name
        let sequence = &sequence[..sequence.len() - 1];

        Ok(Some(completion_response_from(
            sequence,
            &self.account_trie,
        )?))
    }

    fn handle_currency(&self, node: &Node) -> Result<Option<CompletionResponse>> {
        let result = self
            .currency_trie
            .as_ref()
            .unwrap()
            .predictive_search(node_text(node, &self.text)?.chars().collect::<Vec<char>>());

        Ok(Some(CompletionResponse::Array(
            result
                .iter()
                // TODO: enhance with currency info
                .map(|c| CompletionItem::new_simple(c.iter().collect(), "".to_string()))
                .collect(),
        )))
    }

    fn handle_identifier(&self, node: &Node) -> Result<Option<CompletionResponse>> {
        // This happens for initial completions, i.e. if a character has not triggered
        // yet. This means this is likely one of the top-level accounts.
        let identifier = node_text(node, &self.text)?;

        for account in ["Expenses", "Assets", "Liabilities", "Equity", "Revenue"] {
            // Yes, for some stupid reason, the first character is matched as an ERROR
            // and the identifier starts with the second character ...
            if account[1..].starts_with(identifier) {
                return Ok(Some(CompletionResponse::Array(vec![
                    CompletionItem::new_simple(account.to_string(), "".to_string()),
                ])));
            }
        }

        Ok(None)
    }

    fn handle_node(&self, node: &Node) -> Result<Option<CompletionResponse>> {
        match node.kind() {
            "currency" => self.handle_currency(node),
            "identifier" => self.handle_identifier(node),
            "account" => self.handle_account(node),
            _ => Ok(None),
        }
    }
}

struct Backend {
    client: Option<Client>,
    state: Arc<RwLock<State>>,
    language: Language,
}

impl Backend {
    fn new(client: Client) -> Self {
        Self {
            client: Some(client),
            language: tree_sitter_beancount::language(),
            state: Arc::new(RwLock::new(State {
                text: "".to_string(),
                commodities: HashMap::default(),
                account_trie: None,
                currency_trie: None,
            })),
        }
    }
}

impl Backend {
    /// Load ledger to search trie and lines.
    ///
    /// TODO: recursively load included ledgers to retrieve all accounts.
    async fn load_ledgers(&self, uri: &Url) -> anyhow::Result<()> {
        let mut state = self.state.write().await;
        let data = beancount::Data::new(uri)?;

        state.account_trie.insert(data.account_trie());
        state.currency_trie.insert(data.currency_trie());
        state.text = data.text;
        state.commodities = data.commodities;

        Ok(())
    }

    async fn log_message<M: Display>(&self, typ: MessageType, message: M) {
        if let Some(client) = &self.client {
            client.log_message(typ, message).await;
        }
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
                    TextDocumentSyncKind::Full,
                )),
                completion_provider: Some(CompletionOptions {
                    resolve_provider: Some(false),
                    trigger_characters: Some(vec![":".to_string()]),
                    work_done_progress_options: Default::default(),
                    all_commit_characters: None,
                }),
                definition_provider: Some(OneOf::Left(true)),
                ..Default::default()
            },
        })
    }

    async fn initialized(&self, _: InitializedParams) {}

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        if let Err(err) = self.load_ledgers(&params.text_document.uri).await {
            self.log_message(MessageType::Info, err.to_string()).await;
        }
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let mut state = self.state.write().await;
        state.text = params.content_changes[0].text.clone();
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let state = self.state.read().await;

        if state.account_trie.is_none() {
            return Ok(None);
        }

        let mut parser = tree_sitter::Parser::new();
        parser.set_language(self.language).map_err(Error::from)?;

        let tree = parser.parse(&state.text, None).unwrap();

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
                    state.handle_character_triggered(&node)
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

        let tree = parser.parse(&state.text, None).unwrap();

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
                let location = state.commodities.get(node_text(&node, &state.text)?);

                match location {
                    None => {
                        return Ok(None);
                    }
                    Some(location) => {
                        return Ok(Some(GotoDefinitionResponse::Array(vec![location.clone()])));
                    }
                }
            }
        }

        Ok(None)
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}

#[tokio::main]
async fn main() {
    let (service, messages) = LspService::new(Backend::new);

    Server::new(tokio::io::stdin(), tokio::io::stdout())
        .interleave(messages)
        .serve(service)
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;
    use std::io::Write;
    use std::path::Path;

    impl Backend {
        fn new_without_client() -> Self {
            Self {
                client: None,
                language: tree_sitter_beancount::language(),
                state: Arc::new(RwLock::new(State {
                    text: "".to_string(),
                    commodities: HashMap::default(),
                    account_trie: None,
                    currency_trie: None,
                })),
            }
        }
    }

    fn url_from_file_path<P: AsRef<Path>>(path: P) -> anyhow::Result<Url> {
        Ok(Url::from_file_path(path).map_err(|_| anyhow!("Could not create URI"))?)
    }

    #[tokio::test]
    async fn complete_root_account() -> anyhow::Result<()> {
        let mut file = tempfile::NamedTempFile::new()?;

        write!(
            file.as_file_mut(),
            r#"
2021-07-11 "foo" "bar"
  Expe
        "#
        )?;

        let backend = Backend::new_without_client();
        let uri = url_from_file_path(file.path())?;
        backend.load_ledgers(&uri).await?;

        let params = CompletionParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: Position {
                    line: 2,
                    character: 5,
                },
            },
            context: Some(CompletionContext {
                trigger_kind: CompletionTriggerKind::Invoked,
                trigger_character: None,
            }),
            work_done_progress_params: WorkDoneProgressParams {
                work_done_token: None,
            },
            partial_result_params: PartialResultParams {
                partial_result_token: None,
            },
        };

        let result = backend.completion(params).await?.unwrap();

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
    async fn complete_sub_account() -> anyhow::Result<()> {
        let mut file = tempfile::NamedTempFile::new()?;

        write!(
            file.as_file_mut(),
            r#"
2021-07-11 "foo" "bar"
  Expenses:Foo:Bar
  Expenses:Foo:Qux
  Expenses:F
        "#
        )?;

        let backend = Backend::new_without_client();
        let uri = url_from_file_path(file.path())?;
        backend.load_ledgers(&uri).await?;

        let params = CompletionParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: Position {
                    line: 4,
                    character: 12,
                },
            },
            context: Some(CompletionContext {
                trigger_kind: CompletionTriggerKind::Invoked,
                trigger_character: None,
            }),
            work_done_progress_params: WorkDoneProgressParams {
                work_done_token: None,
            },
            partial_result_params: PartialResultParams {
                partial_result_token: None,
            },
        };

        let result = backend.completion(params).await?.unwrap();

        match result {
            CompletionResponse::Array(items) => {
                for item in items {
                    assert!(
                        item.label == "F" || item.label == "Foo:Bar" || item.label == "Foo:Qux"
                    );
                }
            }
            _ => assert!(false),
        };

        Ok(())
    }
}
