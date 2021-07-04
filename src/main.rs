use std::path::Path;
use std::sync::Arc;
use tokio::fs::read_to_string;
use tokio::sync::RwLock;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};
use tree_sitter::Language;
use trie_rs::{Trie, TrieBuilder};

struct State {
    text: String,
    account_trie: Option<Trie<String>>,
    currency_trie: Option<Trie<char>>,
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
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(self.language)?;

        let mut state = self.state.write().await;
        let text = read_to_string(filename).await?;
        let tree = parser.parse(&text, None).unwrap();
        let mut cursor = tree.root_node().walk();
        let mut account_trie_builder = TrieBuilder::new();
        let mut currency_trie_builder = TrieBuilder::new();

        let transactions = tree
            .root_node()
            .children(&mut cursor)
            .filter(|c| c.kind() == "transaction")
            .collect::<Vec<_>>();

        for transaction in transactions {
            let lists = transaction
                .children_by_field_name("posting_or_kv_list", &mut cursor)
                .collect::<Vec<_>>();

            for list in lists {
                let postings = list
                    .children(&mut cursor)
                    .filter(|c| c.kind() == "posting")
                    .collect::<Vec<_>>();

                for posting in postings {
                    for account in posting.children_by_field_name("account", &mut cursor) {
                        account_trie_builder.push(
                            account
                                .utf8_text(&text.as_bytes())?
                                .split(":")
                                .map(|p| p.to_string())
                                .collect::<Vec<String>>(),
                        );
                    }

                    let amounts = posting
                        .children_by_field_name("amount", &mut cursor)
                        .collect::<Vec<_>>();

                    for amount in amounts {
                        for currency in amount
                            .children(&mut cursor)
                            .filter(|c| c.kind() == "currency")
                        {
                            currency_trie_builder.push(
                                currency
                                    .utf8_text(&text.as_bytes())?
                                    .chars()
                                    .collect::<Vec<char>>(),
                            );
                        }
                    }
                }
            }
        }

        state.account_trie.insert(account_trie_builder.build());
        state.currency_trie.insert(currency_trie_builder.build());
        state.text = text;

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
            let account = node
                .unwrap()
                .utf8_text(state.text.as_bytes())
                .unwrap()
                .to_string();

            let sequence: Vec<String> = account
                .split(":")
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .collect();

            let result = state
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
        } else {
            if let Some(node) = node {
                if node.kind() == "currency" {
                    let result = state.currency_trie.as_ref().unwrap().predictive_search(
                        node.utf8_text(state.text.as_bytes())
                            .unwrap()
                            .chars()
                            .collect::<Vec<char>>(),
                    );

                    return Ok(Some(CompletionResponse::Array(
                        result
                            .iter()
                            .map(|c| CompletionItem::new_simple(c.iter().collect(), "".to_string()))
                            .collect(),
                    )));
                }
            }

            return Ok(None);
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
