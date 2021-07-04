# beancount-language-server

A Language Server Protocol (LSP) for Beancount ledgers.


## Installation

Build the binary

    cargo build --release

and copy it to your path, e.g.

    cp target/release/beancount-language-server ~/.local/bin

### Neovim LSP

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
