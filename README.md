# beancount-language-server

[![Rust](https://github.com/matze/beancount-language-server/actions/workflows/rust.yml/badge.svg?branch=master)](https://github.com/matze/beancount-language-server/actions/workflows/rust.yml)

A Language Server Protocol (LSP) server implementation for Beancount ledgers.


## Features

* **Completion**: accounts, payees
* **Formatting**: full file
* **Definitions**: commodities


## Installation

Build the binary

    cargo build --release

and copy it to your path, e.g.

    cp target/release/beancount-language-server ~/.local/bin

### Neovim LSP setup

The official nvim-lspconfig plugin is set up for the [Javascript-based language
server](https://github.com/polarmutex/beancount-language-server), so for now you
have to add an alternative configuration entry like this in your `init.vim`:

```lua
local configs = require 'lspconfig/configs'
local util = require 'lspconfig/util'

configs["beancount_rs"] = {
  default_config = {
    cmd = {"beancount-language-server"};
    filetypes = {"beancount"};
    root_dir = function(fname)
      return util.find_git_ancestor(fname) or util.path.dirname(fname)
    end;
  };
  docs = {
    description = "hello";
    default_config = {
      root_dir = [[root_pattern("elm.json")]];
    };
  };
}

local nvim_lsp = require('lspconfig')
nvim_lsp.beancount_rs.setup({})
```


## License

beancount-language-server is licensed under the MIT license, see
[LICENSE](https://github.com/matze/beancount-language-server/blob/master/LICENSE)
for more information.
