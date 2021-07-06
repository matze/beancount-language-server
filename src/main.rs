use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};
use tree_sitter::{Language, Node};
use trie_rs::Trie;

mod beancount;

struct State {
    text: String,
    account_trie: Option<Trie<String>>,
    currency_trie: Option<Trie<char>>,
}

impl State {
    fn handle_character_triggered(
        &self,
        node: &Option<Node>,
    ) -> Result<Option<CompletionResponse>> {
        let account = node
            .unwrap()
            .utf8_text(self.text.as_bytes())
            .unwrap()
            .to_string();

        let sequence: Vec<String> = account
            .split(":")
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect();

        let result = self
            .account_trie
            .as_ref()
            .unwrap()
            .predictive_search(&sequence);

        let prefix_length = sequence.len();

        Ok(Some(CompletionResponse::Array(
            result
                .iter()
                .map(|seq| {
                    CompletionItem::new_simple(seq[prefix_length..].join(":"), "".to_string())
                })
                .collect(),
        )))
    }

    fn handle_currency(&self, node: &Node) -> Result<Option<CompletionResponse>> {
        let result = self.currency_trie.as_ref().unwrap().predictive_search(
            node.utf8_text(self.text.as_bytes())
                .unwrap()
                .chars()
                .collect::<Vec<char>>(),
        );

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
        let identifier = node.utf8_text(self.text.as_bytes()).unwrap();

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
            _ => Ok(None),
        }
    }
}

struct Backend {
    client: Client,
    state: Arc<RwLock<State>>,
    language: Language,
}

impl Backend {
    fn new(client: Client) -> Self {
        Self {
            client,
            language: tree_sitter_beancount::language(),
            state: Arc::new(RwLock::new(State {
                text: "".to_string(),
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
    async fn load_ledgers(&self, filename: &Path) -> anyhow::Result<()> {
        let mut state = self.state.write().await;
        let data = beancount::Data::new(filename).await?;

        state.account_trie.insert(data.account_trie());
        state.currency_trie.insert(data.currency_trie());
        state.text = data.text;

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
                    TextDocumentSyncKind::Full,
                )),
                completion_provider: Some(CompletionOptions {
                    resolve_provider: Some(false),
                    trigger_characters: Some(vec![":".to_string()]),
                    work_done_progress_options: Default::default(),
                    all_commit_characters: None,
                }),
                ..Default::default()
            },
        })
    }

    async fn initialized(&self, _: InitializedParams) {}

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        if let Err(err) = self
            .load_ledgers(Path::new(&params.text_document.uri.path()))
            .await
        {
            self.client
                .log_message(MessageType::Info, err.to_string())
                .await;
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
        parser.set_language(self.language).unwrap();

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
            .map_or(None, |c| c.trigger_character)
            .map_or(None, |c| if c == ":" { Some(()) } else { None })
            .is_some();

        let node = tree
            .root_node()
            .named_descendant_for_point_range(start, end);

        if is_character_triggered {
            state.handle_character_triggered(&node)
        } else {
            match node {
                Some(node) => state.handle_node(&node),
                None => Ok(None),
            }
        }
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}

#[tokio::main]
async fn main() {
    let (service, messages) = LspService::new(|client| Backend::new(client));

    Server::new(tokio::io::stdin(), tokio::io::stdout())
        .interleave(messages)
        .serve(service)
        .await;
}
