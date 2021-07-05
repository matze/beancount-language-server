use std::path::Path;
use std::collections::HashSet;
use tokio::fs::read_to_string;
use trie_rs::{Trie, TrieBuilder};

pub struct Data {
    pub accounts: HashSet<Vec<String>>,
    pub currencies: HashSet<Vec<char>>,
    pub text: String,
}

impl Data {
    pub async fn new(filename: &Path) -> anyhow::Result<Self> {
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(tree_sitter_beancount::language())?;
        let text = read_to_string(filename).await?;
        let tree = parser.parse(&text, None).unwrap();
        let mut cursor = tree.root_node().walk();

        let mut accounts = HashSet::new();
        let mut currencies = HashSet::new();

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
                        accounts.insert(
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
                            currencies.insert(
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

        Ok(Self {
            accounts,
            currencies,
            text
        })
    }

    pub fn account_trie(&self) -> Trie<String> {
        let mut builder = TrieBuilder::new();

        for account in &self.accounts {
            builder.push(account);
        }

        builder.build()
    }

    pub fn currency_trie(&self) -> Trie<char> {
        let mut builder = TrieBuilder::new();

        for currency in &self.currencies {
            builder.push(currency);
        }

        builder.build()
    }
}