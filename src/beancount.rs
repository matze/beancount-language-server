use crate::Error;
use std::collections::{HashMap, HashSet};
use std::fs::read_to_string;
use std::path::Path;
use tower_lsp::lsp_types::{Location, Position, Range, Url};
use trie_rs::{Trie, TrieBuilder};

#[derive(Default)]
pub struct Data {
    pub commodities: HashMap<String, Location>,
    accounts: HashSet<Vec<String>>,
    currencies: HashSet<Vec<char>>,
    pub text: String,
}

impl Data {
    pub fn new(uri: &Url) -> Result<Self, Error> {
        Data::read(uri, Self::default())
    }

    /// Recursively read ledgers, i.e. those included.
    fn read(uri: &Url, data: Self) -> Result<Self, Error> {
        let file_path = uri.to_file_path().map_err(|_| Error::UriToPathConversion)?;

        let text = read_to_string(&file_path)?;

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

        let mut data = data;

        // Descend into included ledgers, ignore all that fail to load.
        let includes = tree
            .root_node()
            .children(&mut cursor)
            .filter(|c| c.kind() == "include")
            .collect::<Vec<_>>();

        let include_datas = includes.into_iter().filter_map(|include| {
            let maybe_node = include
                .children(&mut cursor)
                .filter(|c| c.kind() == "string")
                .next();

            if maybe_node.is_none() {
                return None;
            }

            let node = maybe_node.unwrap();

            let filename = node
                .utf8_text(&text.as_bytes())
                .unwrap()
                .trim_start_matches('"')
                .trim_end_matches('"');

            let path = Path::new(filename);

            let path = if path.is_absolute() {
                path.to_path_buf()
            } else {
                if file_path.is_absolute() {
                    file_path.parent().unwrap().join(path)
                } else {
                    path.to_path_buf()
                }
            };

            let uri = Url::from_file_path(path).unwrap();
            Some(Data::read(&uri, Data::default()))
        });

        for include_data in include_datas {
            if let Ok(include_data) = include_data {
                data.commodities
                    .extend(include_data.commodities.into_iter());
                data.accounts.extend(include_data.accounts.into_iter());
                data.currencies.extend(include_data.currencies.into_iter());
            }
        }

        data.commodities.extend(commodities.into_iter());
        data.accounts.extend(accounts.into_iter());
        data.currencies.extend(currencies.into_iter());
        data.text = text; // TODO: yeah ...

        Ok(data)
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
    use std::fs::File;
    use std::io::Write;
    use std::path::Path;
    use tower_lsp::lsp_types::Url;

    fn url_from_file_path<P: AsRef<Path>>(path: P) -> Result<Url, Error> {
        Ok(Url::from_file_path(path).map_err(|_| Error::UriToPathConversion)?)
    }

    #[test]
    fn parse() -> Result<(), Error> {
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
    fn commodity_definition() -> Result<(), Error> {
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

    #[test]
    fn include() -> Result<(), Error> {
        let dir = tempfile::tempdir()?;

        let commodity_file_path = dir.path().join("commodities.beancount");
        let mut commodity_file = File::create(&commodity_file_path)?;

        write!(
            commodity_file,
            r#"
        2015-01-01 commodity USD
          name: "US Dollar"
          type: "Currency"
        "#
        )?;

        let main_file_path = dir.path().join("main.beancount");
        let mut main_file = File::create(&main_file_path)?;

        write!(
            main_file,
            r#"
        include "commodities.beancount"

        2021-07-10 "foo" "bar"
            Expenses:Cash       100.00 USD
            Assets:Checking    -100.00 USD
        "#
        )?;

        let data = Data::new(&url_from_file_path(&main_file_path)?)?;
        let usd_location = data.commodities.get("USD").unwrap();
        assert_eq!(usd_location.uri.path(), commodity_file_path.as_os_str());

        Ok(())
    }
}
