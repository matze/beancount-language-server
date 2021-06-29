use beancount_core::Directive;
use beancount_parser::parse;
use std::path::Path;
use std::sync::Arc;
use tokio::fs::read_to_string;
use tokio::sync::RwLock;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};
use trie_rs::{Trie, TrieBuilder};

struct State {
    lines: Vec<String>,
    trie_builder: TrieBuilder<String>,
    trie: Option<Trie<String>>,
}

struct Backend {
    client: Client,
    state: Arc<RwLock<State>>,
}

impl Backend {
    fn new(client: Client) -> Self {
        Self {
            client,
            state: Arc::new(RwLock::new(State {
                lines: vec![],
                trie_builder: TrieBuilder::new(),
                trie: None,
            })),
        }
    }
}

impl Backend {
    /// Load ledger to search trie and lines.
    ///
    /// TODO: recursively load included ledgers to retrieve all accounts.
    async fn load_ledgers(&self, filename: &Path) -> anyhow::Result<()> {
        let content = read_to_string(filename).await?;
        let ledger = parse(&content)?;

        let mut state = self.state.write().await;

        for postings in ledger.directives.iter().filter_map(|d| match d {
            Directive::Transaction(txn) => Some(&txn.postings),
            _ => None,
        }) {
            for posting in postings {
                let mut sequence = vec![posting.account.ty.default_name().to_string()];

                for part in &posting.account.parts {
                    sequence.push(part.to_string());
                }

                state.trie_builder.push(sequence);
            }
        }

        let trie = state.trie_builder.build();
        state.trie.insert(trie);
        state.lines = content.split("\n").map(|line| line.to_string()).collect();

        Ok(())
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            server_info: Some(ServerInfo {
                name: "beancount-language-server".to_string(),
                version: Some("0.0".to_string()),
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

        state.lines = params.content_changes[0]
            .text
            .split("\n")
            .map(|line| line.to_string())
            .collect();
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        if params
            .context
            .map_or(None, |c| c.trigger_character)
            .map_or(None, |c| if c == ":" { Some(()) } else { None }).is_none()
        {
            return Ok(None);
        }

        let state = self.state.read().await;

        if state.trie.is_none() {
            return Ok(None);
        }

        let line_index = params.text_document_position.position.line as usize;
        let char_index = params.text_document_position.position.character as usize - 1;
        let line = &state.lines[line_index][..char_index];
        let start = line.rfind(char::is_whitespace).unwrap_or(0) + 1;
        let line = &state.lines[line_index][start..char_index];
        let sequence: Vec<String> = line.split(":").map(|s| s.to_string()).collect();
        let result = state.trie.as_ref().unwrap().predictive_search(&sequence);
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
