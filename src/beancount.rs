use crate::Error;
use std::collections::{HashMap, HashSet};
use std::fs::read_to_string;
use std::path::Path;
use tower_lsp::lsp_types::{Location, Position, Range, Url};
use tree_sitter::{Node, TreeCursor};
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

fn reformat_postings(postings: &Node, text: &str) -> String {
    let mut cursor = postings.walk();

    let postings = postings.children(&mut cursor).collect::<Vec<_>>();

    let formatted = postings
        .into_iter()
        .map(|p| {
            let account = p
                .child_by_field_name("account")
                .unwrap()
                .utf8_text(text.as_bytes())
                .unwrap();
            let mut amount_children = p
                .child_by_field_name("amount")
                .unwrap()
                .children(&mut cursor);
            assert_eq!(amount_children.len(), 2);

            let number = amount_children
                .next()
                .unwrap()
                .utf8_text(text.as_bytes())
                .unwrap();

            // We want to align so that the number period is always at column position 50. Hence we
            // have to pad with 50 - 2 spaces before account - 1 space after account - 1 period -
            // length of account.
            let period_position = number.find('.').unwrap();
            let numerator = &number[..period_position];
            let denominator = &number[period_position + 1..];
            let width = 50 - 4 - account.len();

            let currency = amount_children
                .next()
                .unwrap()
                .utf8_text(text.as_bytes())
                .unwrap();

            format!(
                "  {} {:>width$}.{} {}",
                account,
                numerator,
                denominator,
                currency,
                width = width
            )
        })
        .collect::<Vec<_>>();

    formatted.join("\n")
}

fn reformat_top_level(cursor: &mut TreeCursor, text: &str) -> String {
    let node = cursor.node();
    let end_point = node.range().end_point;

    let newlines = |cursor: &TreeCursor| -> String {
        let current = cursor.node().range();
        "\n".repeat(current.start_point.row - end_point.row + 1)
    };

    match node.kind() {
        "file" => {
            if cursor.goto_first_child() {
                reformat_top_level(cursor, text)
            } else {
                "".to_string()
            }
        }
        "option" => {
            let key = node
                .child_by_field_name("key")
                .unwrap()
                .utf8_text(text.as_bytes())
                .unwrap();

            let value = node
                .child_by_field_name("value")
                .unwrap()
                .utf8_text(text.as_bytes())
                .unwrap();

            if cursor.goto_next_sibling() {
                format!(
                    "option {} {}{}{}",
                    key,
                    value,
                    newlines(cursor),
                    reformat_top_level(cursor, text)
                )
            } else {
                format!("option {} {}", key, value)
            }
        }
        "plugin" => {
            let plugin = node.child(1).unwrap().utf8_text(text.as_bytes()).unwrap();

            if cursor.goto_next_sibling() {
                format!(
                    "plugin {}{}{}",
                    plugin,
                    newlines(cursor),
                    reformat_top_level(cursor, text)
                )
            } else {
                format!("plugin {}", plugin)
            }
        }
        "include" => {
            let include = node.child(1).unwrap().utf8_text(text.as_bytes()).unwrap();

            if cursor.goto_next_sibling() {
                format!(
                    "include {}{}{}",
                    include,
                    newlines(cursor),
                    reformat_top_level(cursor, text)
                )
            } else {
                format!("include {}", include)
            }
        }
        "transaction" => {
            let date = node
                .child_by_field_name("date")
                .unwrap()
                .utf8_text(text.as_bytes())
                .unwrap();

            let txn = node
                .child_by_field_name("txn")
                .unwrap()
                .utf8_text(text.as_bytes())
                .unwrap();

            let txn_strings = node
                .child_by_field_name("txn_strings")
                .unwrap()
                .children(cursor)
                .collect::<Vec<_>>();

            assert_eq!(txn_strings.len(), 2);
            let payee = txn_strings[0].utf8_text(text.as_bytes()).unwrap();
            let narration = txn_strings[1].utf8_text(text.as_bytes()).unwrap();

            let posting = node.child_by_field_name("posting_or_kv_list").unwrap();

            if cursor.goto_next_sibling() {
                format!(
                    "{} {} {} {}\n{}{}{}",
                    date,
                    txn,
                    payee,
                    narration,
                    reformat_postings(&posting, text),
                    newlines(cursor),
                    reformat_top_level(cursor, text)
                )
            } else {
                format!(
                    "{} {} {} {}\n{}",
                    date,
                    txn,
                    payee,
                    narration,
                    reformat_postings(&posting, text)
                )
            }
        }
        _ => "".to_string(),
    }
}

pub fn reformat(uri: &Url) -> Result<Option<String>, Error> {
    let file_path = uri.to_file_path().map_err(|_| Error::UriToPathConversion)?;
    let text = read_to_string(&file_path)?;

    let mut parser = tree_sitter::Parser::new();
    parser.set_language(tree_sitter_beancount::language())?;
    let tree = parser.parse(&text, None).unwrap();
    let mut cursor = tree.root_node().walk();

    Ok(Some(reformat_top_level(&mut cursor, &text)))
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

        2021-07-10 * "foo" "bar"
            Expenses:Cash       100.00 USD
            Assets:Checking    -100.00 USD
        "#
        )?;

        let data = Data::new(&url_from_file_path(&main_file_path)?)?;
        let usd_location = data.commodities.get("USD").unwrap();
        assert_eq!(usd_location.uri.path(), commodity_file_path.as_os_str());

        Ok(())
    }

    #[test]
    fn reformat_top_level() -> Result<(), Error> {
        let mut file = tempfile::NamedTempFile::new()?;

        write!(
            file.as_file_mut(),
            r#"option "operating_currency" "EUR"

  plugin "beancount.plugins.implicit_prices"
plugin    "beancount.plugins.check_commodity"

include "commodities.beancount"   "#
        )?;

        let reformatted = super::reformat(&url_from_file_path(file.path())?)?;
        assert!(reformatted.is_some());
        let reformatted = reformatted.unwrap();

        let expected = r#"option "operating_currency" "EUR"

plugin "beancount.plugins.implicit_prices"
plugin "beancount.plugins.check_commodity"

include "commodities.beancount""#;

        assert_eq!(reformatted, expected);
        Ok(())
    }

    #[test]
    fn reformat_transaction() -> Result<(), Error> {
        let mut file = tempfile::NamedTempFile::new()?;

        write!(
            file.as_file_mut(),
            r#"2021-07-10  ! "foo"     "bar"
 Expenses:Cash             100.00 EUR
   Assets:Checking    -100.00 EUR
        "#
        )?;

        let reformatted = super::reformat(&url_from_file_path(file.path())?)?;
        assert!(reformatted.is_some());
        let reformatted = reformatted.unwrap();

        let expected = r#"2021-07-10 ! "foo" "bar"
  Expenses:Cash                               100.00 EUR
  Assets:Checking                            -100.00 EUR"#;
        assert_eq!(reformatted, expected);

        Ok(())
    }
}
