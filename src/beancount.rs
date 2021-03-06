use crate::Error;
use std::collections::{HashMap, HashSet};
use std::fs::read_to_string;
use std::path::Path;
use tower_lsp::lsp_types::{Location, Position, Range, Url};
use tree_sitter::{Node, TreeCursor};

#[derive(Default)]
pub struct Data {
    pub commodities: HashMap<String, Location>,
    pub accounts: HashSet<String>,
    pub currencies: HashSet<String>,
    pub payees: HashSet<String>,
    pub text: String,
}

fn node_text_by_field_name<'a>(
    node: &'a Node,
    field_name: &'a str,
    bytes: &'a [u8],
) -> Result<&'a str, Error> {
    Ok(node
        .child_by_field_name(field_name)
        .ok_or_else(|| Error::InvalidState)?
        .utf8_text(bytes)?)
}

impl Data {
    pub fn new(uri: &Url) -> Result<Self, Error> {
        Data::read(uri, Self::default())
    }

    /// Recursively read ledgers, i.e. those included.
    fn read(uri: &Url, data: Self) -> Result<Self, Error> {
        let file_path = uri.to_file_path().map_err(|_| Error::UriToPathConversion)?;

        let text = read_to_string(&file_path)?;
        let bytes = text.as_bytes();

        let mut parser = tree_sitter::Parser::new();
        parser.set_language(tree_sitter_beancount::language())?;
        let tree = parser
            .parse(&text, None)
            .ok_or_else(|| Error::TreeParseError)?;
        let mut cursor = tree.root_node().walk();

        let mut commodities = HashMap::new();

        for commodity in tree
            .root_node()
            .children(&mut cursor)
            .filter(|c| c.kind() == "commodity")
        {
            let currency = node_text_by_field_name(&commodity, "currency", bytes)?;
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
        let mut payees = HashSet::new();

        let transactions = tree
            .root_node()
            .children(&mut cursor)
            .filter(|c| c.kind() == "transaction")
            .collect::<Vec<_>>();

        for transaction in transactions {
            if let Some(txn_strings) = transaction.child_by_field_name("txn_strings") {
                if let Some(payee) = txn_strings.children(&mut cursor).next() {
                    payees.insert(
                        payee
                            .utf8_text(bytes)?
                            .trim_end_matches('"')
                            .trim_start_matches('"')
                            .to_string(),
                    );
                }
            }

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
                        accounts.insert(account.utf8_text(bytes)?.to_string());
                    }

                    let amounts = posting
                        .children_by_field_name("amount", &mut cursor)
                        .collect::<Vec<_>>();

                    for amount in amounts {
                        for currency in amount
                            .children(&mut cursor)
                            .filter(|c| c.kind() == "currency")
                        {
                            currencies.insert(currency.utf8_text(bytes)?.to_string());
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
            let node = include
                .children(&mut cursor)
                .find(|c| c.kind() == "string")?;

            let filename = node
                .utf8_text(bytes)
                .unwrap()
                .trim_start_matches('"')
                .trim_end_matches('"');

            let path = Path::new(filename);

            let path = if path.is_absolute() {
                path.to_path_buf()
            } else if file_path.is_absolute() {
                file_path.parent().unwrap().join(path)
            } else {
                path.to_path_buf()
            };

            let uri = Url::from_file_path(path).unwrap();
            Some(Data::read(&uri, Data::default()))
        });

        for include_data in include_datas.flatten() {
            data.commodities
                .extend(include_data.commodities.into_iter());
            data.accounts.extend(include_data.accounts.into_iter());
            data.currencies.extend(include_data.currencies.into_iter());
        }

        data.commodities.extend(commodities.into_iter());
        data.accounts.extend(accounts.into_iter());
        data.currencies.extend(currencies.into_iter());
        data.payees.extend(payees.into_iter());
        data.text = text; // TODO: yeah ...

        Ok(data)
    }
}

fn reformat_postings(postings: &Node, text: &str) -> Result<String, Error> {
    let mut cursor = postings.walk();
    let bytes = text.as_bytes();

    let comments = postings
        .children(&mut cursor)
        .filter(|p| p.kind() == "comment")
        .map(|p| Ok::<_, Error>(format!("  {}\n", p.utf8_text(bytes)?)))
        .collect::<Result<Vec<_>, _>>()?;

    let postings = postings
        .children(&mut cursor)
        .filter(|p| p.kind() == "posting")
        .collect::<Vec<_>>();

    let formatted = postings
        .into_iter()
        .map(|p| {
            let account = p
                .child_by_field_name("account")
                .ok_or_else(|| Error::InvalidState)?
                .utf8_text(bytes)?;

            let amount = {
                match p.child_by_field_name("amount") {
                    Some(amount) => {
                        let mut amount_children = amount.children(&mut cursor);

                        assert_eq!(amount_children.len(), 2);

                        let number = amount_children
                            .next()
                            .ok_or_else(|| Error::InvalidState)?
                            .utf8_text(bytes)?;

                        let width = 50 - 4 - account.len();

                        let currency = amount_children
                            .next()
                            .ok_or_else(|| Error::InvalidState)?
                            .utf8_text(bytes)?;

                        // We want to align so that the number period is always at column position 50. Hence we
                        // have to pad with 50 - 2 spaces before account - 1 space after account - 1 period -
                        // length of account.
                        match number.find('.') {
                            Some(position) => {
                                let numerator = &number[..position];
                                let denominator = &number[position + 1..];
                                format!(
                                    " {:>width$}.{} {}",
                                    numerator,
                                    denominator,
                                    currency,
                                    width = width
                                )
                            }
                            None => format!(" {:>width$} {}", number, currency, width = width),
                        }
                    }
                    None => "".to_string(),
                }
            };

            let cost = match p.child_by_field_name("cost_spec") {
                Some(cost) => format!(" {}", cost.utf8_text(bytes)?),
                None => "".to_string(),
            };

            let comment = match p.child_by_field_name("comment") {
                Some(comment) => format!("  {}", comment.utf8_text(bytes)?),
                None => "".to_string(),
            };

            Ok::<_, Error>(format!("  {}{}{}{}", account, amount, cost, comment))
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok(format!("{}{}", comments.join("\n"), formatted.join("\n")))
}

fn reformat_top_level(cursor: &mut TreeCursor, text: &str) -> Result<String, Error> {
    let node = cursor.node();
    let bytes = text.as_bytes();
    let end_point = node.range().end_point;

    let newlines = |cursor: &TreeCursor| -> String {
        let current = cursor.node().range();
        "\n".repeat(current.start_point.row - end_point.row + 1)
    };

    let formatted = match node.kind() {
        "file" => {
            if cursor.goto_first_child() {
                reformat_top_level(cursor, text)?
            } else {
                "".to_string()
            }
        }
        "option" => {
            let key = node_text_by_field_name(&node, "key", bytes)?;
            let value = node_text_by_field_name(&node, "value", bytes)?;

            if cursor.goto_next_sibling() {
                format!(
                    "option {} {}{}{}",
                    key,
                    value,
                    newlines(cursor),
                    reformat_top_level(cursor, text)?
                )
            } else {
                format!("option {} {}", key, value)
            }
        }
        "plugin" => {
            let plugin = node.child(1).unwrap().utf8_text(bytes)?;

            if cursor.goto_next_sibling() {
                format!(
                    "plugin {}{}{}",
                    plugin,
                    newlines(cursor),
                    reformat_top_level(cursor, text)?
                )
            } else {
                format!("plugin {}", plugin)
            }
        }
        "open" => {
            let date = node_text_by_field_name(&node, "date", bytes)?;
            let account = node_text_by_field_name(&node, "account", bytes)?;

            if cursor.goto_next_sibling() {
                format!(
                    "{} open {}{}{}",
                    date,
                    account,
                    newlines(cursor),
                    reformat_top_level(cursor, text)?
                )
            } else {
                format!("{} open {}", date, account)
            }
        }
        "include" => {
            let include = node.child(1).unwrap().utf8_text(bytes)?;

            if cursor.goto_next_sibling() {
                format!(
                    "include {}{}{}",
                    include,
                    newlines(cursor),
                    reformat_top_level(cursor, text)?
                )
            } else {
                format!("include {}", include)
            }
        }
        "transaction" => {
            let date = node_text_by_field_name(&node, "date", bytes)?;
            let txn = node_text_by_field_name(&node, "txn", bytes)?;

            let txn_strings = node
                .child_by_field_name("txn_strings")
                .ok_or_else(|| Error::InvalidState)?
                .children(&mut node.walk())
                .collect::<Vec<_>>();

            let payee = txn_strings[0].utf8_text(bytes)?;

            let first_line = match txn_strings.len() {
                1 => {
                    format!("{} {} {}", date, txn, payee)
                }
                2 => {
                    let narration = txn_strings[1].utf8_text(bytes)?;
                    format!("{} {} {} {}", date, txn, payee, narration)
                }
                _ => return Err(Error::UnexpectedFormat),
            };

            let posting = node
                .child_by_field_name("posting_or_kv_list")
                .ok_or_else(|| Error::InvalidState)?;

            if cursor.goto_next_sibling() {
                format!(
                    "{}\n{}{}{}",
                    first_line,
                    reformat_postings(&posting, text)?,
                    newlines(cursor),
                    reformat_top_level(cursor, text)?
                )
            } else {
                format!("{}\n{}", first_line, reformat_postings(&posting, text)?)
            }
        }
        "\n" => {
            if cursor.goto_next_sibling() {
                format!("\n{}", reformat_top_level(cursor, text)?)
            } else {
                "".to_string()
            }
        }
        _ => "".to_string(),
    };

    Ok(formatted)
}

pub fn reformat(uri: &Url) -> Result<Option<String>, Error> {
    let file_path = uri.to_file_path().map_err(|_| Error::UriToPathConversion)?;
    let text = read_to_string(&file_path)?;

    let mut parser = tree_sitter::Parser::new();
    parser.set_language(tree_sitter_beancount::language())?;

    let tree = parser
        .parse(&text, None)
        .ok_or_else(|| Error::TreeParseError)?;

    let mut cursor = tree.root_node().walk();

    Ok(Some(reformat_top_level(&mut cursor, &text)?))
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

    fn reformat<P: AsRef<Path>>(path: P) -> Result<String, Error> {
        Ok(super::reformat(&url_from_file_path(path)?)?.ok_or_else(|| Error::UnexpectedFormat)?)
    }

    #[test]
    fn parse() -> Result<(), Error> {
        let mut file = tempfile::NamedTempFile::new()?;

        write!(
            file.as_file_mut(),
            r#"2021-07-10 "foo" "bar"
  Expenses:Cash       100.00 EUR
  Assets:Checking    -100.00 EUR
        "#
        )?;

        let data = Data::new(&url_from_file_path(file.path())?)?;

        assert_eq!(data.accounts.len(), 2);

        assert!(data.accounts.contains("Expenses:Cash"));
        assert!(data.accounts.contains("Assets:Checking"));

        assert_eq!(data.currencies.len(), 1);
        assert!(data.currencies.contains("EUR"));

        assert_eq!(data.payees.len(), 1);
        assert!(data.payees.contains("foo"));

        Ok(())
    }

    #[test]
    fn commodity_definition() -> Result<(), Error> {
        let mut file = tempfile::NamedTempFile::new()?;

        write!(
            file.as_file_mut(),
            r#"2015-01-01 commodity USD
  name: "US Dollar"
  type: "Currency"
2015-01-01 commodity EUR
  name: "Euro"
  type: "Currency""#
        )?;

        let data = Data::new(&url_from_file_path(file.path())?)?;

        assert_eq!(data.commodities.len(), 2);

        let usd_location = data.commodities.get("USD").unwrap();
        assert_eq!(usd_location.range.start.line, 0);

        let eur_location = data.commodities.get("EUR").unwrap();
        assert_eq!(eur_location.range.start.line, 3);

        Ok(())
    }

    #[test]
    fn include() -> Result<(), Error> {
        let dir = tempfile::tempdir()?;

        let commodity_file_path = dir.path().join("commodities.beancount");
        let mut commodity_file = File::create(&commodity_file_path)?;

        write!(
            commodity_file,
            r#"2015-01-01 commodity USD
  name: "US Dollar"
  type: "Currency""#
        )?;

        let main_file_path = dir.path().join("main.beancount");
        let mut main_file = File::create(&main_file_path)?;

        write!(
            main_file,
            r#"include "commodities.beancount"

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

include "commodities.beancount"  

  2015-01-02 open Expenses:Foo:Bar

"#
        )?;

        let reformatted = reformat(file.path())?;
        let expected = r#"option "operating_currency" "EUR"

plugin "beancount.plugins.implicit_prices"
plugin "beancount.plugins.check_commodity"

include "commodities.beancount"

2015-01-02 open Expenses:Foo:Bar
"#;

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

        let reformatted = reformat(file.path())?;
        let expected = r#"2021-07-10 ! "foo" "bar"
  Expenses:Cash                               100.00 EUR
  Assets:Checking                            -100.00 EUR
"#;

        assert_eq!(reformatted, expected);

        Ok(())
    }

    #[test]
    fn reformat_transaction_with_comment() -> Result<(), Error> {
        let mut file = tempfile::NamedTempFile::new()?;

        write!(
            file.as_file_mut(),
            r#"2021-07-10  ! "foo"     "bar"
    ; some comment
 Expenses:Cash             100.00 EUR
   Assets:Checking    -100.00 EUR

        "#
        )?;

        let reformatted = reformat(file.path())?;
        let expected = r#"2021-07-10 ! "foo" "bar"
  ; some comment
  Expenses:Cash                               100.00 EUR
  Assets:Checking                            -100.00 EUR
"#;

        assert_eq!(reformatted, expected);

        Ok(())
    }

    #[test]
    fn reformat_transaction_without_narration() -> Result<(), Error> {
        let mut file = tempfile::NamedTempFile::new()?;

        write!(
            file.as_file_mut(),
            r#"2021-07-10  * "foo"
 Expenses:Cash             100.00 EUR
   Assets:Checking    -100.00 EUR

        "#
        )?;

        let reformatted = reformat(file.path())?;
        let expected = r#"2021-07-10 * "foo"
  Expenses:Cash                               100.00 EUR
  Assets:Checking                            -100.00 EUR
"#;

        assert_eq!(reformatted, expected);

        Ok(())
    }

    #[test]
    fn reformat_multiple() -> Result<(), Error> {
        let mut file = tempfile::NamedTempFile::new()?;

        write!(
            file.as_file_mut(),
            r#" option   "operating_currency" "EUR"

2021-07-10  * "foo"     "bar"
 Expenses:Cash             100.00 EUR
   Assets:Checking    -100.00 EUR
2021-07-11  ! "foo"   "bar"
 Expenses:Cash              99.00 EUR
   Assets:Checking    -99.00 EUR

        "#
        )?;

        let reformatted = reformat(file.path())?;
        let expected = r#"option "operating_currency" "EUR"

2021-07-10 * "foo" "bar"
  Expenses:Cash                               100.00 EUR
  Assets:Checking                            -100.00 EUR
2021-07-11 ! "foo" "bar"
  Expenses:Cash                                99.00 EUR
  Assets:Checking                             -99.00 EUR
"#;

        assert_eq!(reformatted, expected);

        Ok(())
    }

    #[test]
    fn reformat_comment() -> Result<(), Error> {
        let mut file = tempfile::NamedTempFile::new()?;

        write!(
            file.as_file_mut(),
            r#" 2021-07-10  * "foo"     "bar"
 Expenses:Cash             100.00 EUR ; foo
   Assets:Checking    -100.00 EUR       ; bar

        "#
        )?;

        let reformatted = reformat(file.path())?;
        let expected = r#"2021-07-10 * "foo" "bar"
  Expenses:Cash                               100.00 EUR  ; foo
  Assets:Checking                            -100.00 EUR  ; bar
"#;

        assert_eq!(reformatted, expected);

        Ok(())
    }

    #[test]
    fn reformat_security() -> Result<(), Error> {
        let mut file = tempfile::NamedTempFile::new()?;

        write!(
            file.as_file_mut(),
            r#" 2021-07-10  * "foo"     "bar"
 Assets:Cash             100.00 EUR
   Assets:AAPL    1 AAPL {{100.00 EUR}}

        "#
        )?;

        let reformatted = reformat(file.path())?;
        let expected = r#"2021-07-10 * "foo" "bar"
  Assets:Cash                                 100.00 EUR
  Assets:AAPL                                   1 AAPL {100.00 EUR}
"#;

        assert_eq!(reformatted, expected);

        Ok(())
    }

    #[test]
    fn reformat_no_amount() -> Result<(), Error> {
        let mut file = tempfile::NamedTempFile::new()?;

        write!(
            file.as_file_mut(),
            r#" 2021-07-10  * "foo"     "bar"
 Expenses:Cash             100.00 EUR ; foo
   Assets:Checking

        "#
        )?;

        let reformatted = reformat(file.path())?;
        let expected = r#"2021-07-10 * "foo" "bar"
  Expenses:Cash                               100.00 EUR  ; foo
  Assets:Checking
"#;

        assert_eq!(reformatted, expected);

        Ok(())
    }
}
