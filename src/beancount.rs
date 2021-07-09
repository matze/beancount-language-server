use anyhow::anyhow;
use std::collections::{HashMap, HashSet};
use std::fs::read_to_string;
use tower_lsp::lsp_types::{Location, Position, Range, Url};
use trie_rs::{Trie, TrieBuilder};

pub struct Data {
    pub commodities: HashMap<String, Location>,
    accounts: HashSet<Vec<String>>,
    currencies: HashSet<Vec<char>>,
    pub text: String,
}

impl Data {
    pub fn new(uri: &Url) -> anyhow::Result<Self> {
        let file_path = uri
            .to_file_path()
            .map_err(|err| anyhow!(format!("{:?}", err)))?;

        let text = read_to_string(file_path)?;

        let mut parser = tree_sitter::Parser::new();
        parser.set_language(tree_sitter_beancount::language())?;
        let tree = parser.parse(&text, None).unwrap();
        let mut cursor = tree.root_node().walk();

        let mut commodities = HashMap::new();

        for commodity in tree
            .root_node()
            .children(&mut cursor)
            .filter(|c| c.kind() == "commodity")
        {
            let currency = commodity
                .child_by_field_name("currency")
                .unwrap()
                .utf8_text(&text.as_bytes())
                .unwrap();

            let start = commodity.start_position();
            let end = commodity.end_position();

            let range = Range {
                start: Position {
                    line: start.row as u32,
                    character: start.column as u32,
                },
                end: Position {
                    line: end.row as u32,
                    character: end.column as u32,
                },
            };

            let location = Location {
                uri: (*uri).clone(),
                range,
            };

            commodities.insert(currency.to_string(), location);
        }

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
                                .split(':')
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
            commodities,
            accounts,
            currencies,
            text,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::Path;
    use tower_lsp::lsp_types::Url;

    fn url_from_file_path<P: AsRef<Path>>(path: P) -> anyhow::Result<Url> {
        Ok(Url::from_file_path(path).map_err(|_| anyhow!("Could not create URI"))?)
    }

    #[test]
    fn parse() -> anyhow::Result<()> {
        let mut file = tempfile::NamedTempFile::new()?;

        write!(
            file.as_file_mut(),
            r#"
        2021-07-10 "foo" "bar"
            Expenses:Cash       100.00 EUR
            Assets:Checking    -100.00 EUR
        "#
        )?;

        let data = Data::new(&url_from_file_path(file.path())?)?;

        assert_eq!(data.accounts.len(), 2);

        assert!(data
            .accounts
            .contains(&vec!["Expenses".to_string(), "Cash".to_string()]));
        assert!(data
            .accounts
            .contains(&vec!["Assets".to_string(), "Checking".to_string()]));

        assert_eq!(data.currencies.len(), 1);
        assert!(data.currencies.contains(&vec!['E', 'U', 'R']));

        Ok(())
    }

    #[test]
    fn commodity_definition() -> anyhow::Result<()> {
        let mut file = tempfile::NamedTempFile::new()?;

        write!(
            file.as_file_mut(),
            r#"
        2015-01-01 commodity USD
          name: "US Dollar"
          type: "Currency"
        2015-01-01 commodity EUR
          name: "Euro"
          type: "Currency"
        "#
        )?;

        let data = Data::new(&url_from_file_path(file.path())?)?;

        assert_eq!(data.commodities.len(), 2);

        let usd_location = data.commodities.get("USD").unwrap();
        assert_eq!(usd_location.range.start.line, 1);

        let eur_location = data.commodities.get("EUR").unwrap();
        assert_eq!(eur_location.range.start.line, 4);

        Ok(())
    }
}
