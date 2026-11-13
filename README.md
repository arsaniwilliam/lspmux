# lspmux &emsp; [![Latest Version]][crates.io]

[Latest Version]: https://img.shields.io/crates/v/lspmux.svg
[crates.io]: https://crates.io/crates/lspmux

Multiplexer for [LSP], allows multiple LSP clients (editor windows) to share a
single langauge server instance per workspace.

[LSP]: https://microsoft.github.io/language-server-protocol


## How it works

`lspmux` acts as a proxy between a language server and a LSP client. it is
composed of two parts.

1. server which listens on a socket, takes care of the actual language server
   lifecycle and multiplexes connections from multiple clients onto the
   appropriate language servers. and
2. client shim which acts like a language server binary but only connects to the
   `lspmux` server and forwards all input and output from stdio to the socket.

Some clients, like `nvim`, are able to connect directly to a socket and setup
custom LSP initOptions, for those clients the shim is optional as they can be
configured to connect directly to the `lspmux` server.

When the client shim is invoked by an editor, it finds which workspace and
environment it has been spawned in. It then connects to the server and passes
its environment along during the handshake, the server then either connects it
to an existing language server if one with matching workspace and environment is
running or spawn a new one.

Because neither LSP nor language servers (usually) support multiple clients per
server `lspmux` intercepts the handshake process and modifies IDs of requests
and responses to track which response belongs to which client. Because not all
messages can be tracked this way it drops some, notably it drops any requests
from the server, this may cause some issues for you, if you run into any issues
which are definitely not present in the language server alone please open an
issue!


## How to use

Install `lspmux` using cargo

```sh
$ cargo install lspmux
```

or using one of the package managers which has it
[available in its repository](https://repology.org/project/ra-multiplex/versions).

Run `lspmux` in server mode, make sure that `lspmux` is in your
`PATH`:

```sh
$ which rust-analyzer
/home/user/.cargo/bin/rust-analyzer
$ target/release/lspmux --help
share one language server instance between multiple LSP clients to save resources

Usage: lspmux [COMMAND]

Commands:
  client  Connect to an lspmux server [default]
  server  Start a lpsmux server
  status  Print server status
  config  Print server configuration
  reload  Reload workspace
  help    Print this message or the help of the given subcommand(s)

Options:
  -h, --help     Print help
  -V, --version  Print version
```

On linux distros with systemd `lspmux server` can run as a systemd user
service, see the example `lspmux.service`.
On MacOS it can run as a Launchd service, see the example `lspmux.plist`.

Configure your editor to use `lspmux` as `rust-analyzer`, for example for
CoC in neovim edit `~/.config/nvim/coc-settings.json`, add:

```json
{
    "rust-analyzer.serverPath": "/path/to/lspmux"
}
```

If your editor can connect to a language server via TCP you don't need to use
the `lspmux` client and connect directly to the server but you need to
provide the same information as the proxy command would. See the
[example config for neovim](examples/neovim/init.lua) for details.


## Configuration

Configuration is stored in a TOML file in your system's default configuration
directory, for example `~/.config/lspmux/config.toml`. If you're not sure where
that is on your system starting `lspmux` without a config file present will
print a notice with the expected path.

Note that the configuration file is likely not necessary and `lspmux` should be
usable with all defaults.

Example configuration file:

```toml
# this is an example configuration file for lspmux
#
# all configuration options here are set to their default value they'll have if
# they're not present in the file or if the config file is missing completely.

# time in seconds after which a rust-analyzer server instance with no clients
# connected will get killed to save system memory.
#
# you can set this option to `false` for infinite timeout
instance_timeout = 300 # after 5 minutes

# time in seconds how long to wait between the gc task checks for disconnected
# clients and possibly starts a timeout task. the value must be at least 1.
gc_interval = 10 # every 10 seconds

# ip address and port on which lspmux server listens
# or unix socket path on *nix operating systems
#
# the default "127.0.0.1" only allows connections from localhost which is
# preferred since the protocol doesn't worry about security.
# lspmux server expects the filesystem structure and contents to be the same
# on its machine as on lspmux's machine. if you want to run the server on a
# different computer it's theoretically possible but at least for now you're on
# your own.
#
# ports below 1024 will typically require root privileges and should be
# avoided, the default was picked at random, this only needs to change if
# another application happens to collide with lspmux.
listen = ["127.0.0.1", 27631] # localhost & some random unprivileged port
# listen = "/var/run/lspmux/lspmux.sock" # unix socket

# ip address and port to which lspmux will connect to or unix socket path on
# *nix operating systems
#
# this should usually just match the value of `listen`
connect = ["127.0.0.1", 27631] # same as `listen`
# connect = "/var/run/lspmux/lspmux.sock" # same as `listen`

# default log filters
#
# RUST_LOG env variable overrides this option, both use the same syntax which
# is documented in the `env_logger` documentation here:
# <https://docs.rs/env_logger/0.9.0/env_logger/index.html#enabling-logging>
log_filters = "info"

# environment variable names passed from `lspmux client` to the server
#
# By default no variables are passed and all servers are spawned in the same
# environment as the `lspmux server` is.
# When a name like "LD_LIBRARY_PATH" is specified, the proxy reads the variable
# value from its environment and passes the variable with the value set in the
# proxy environment to the server, which then passes it further to the server
# executable.
#
# If "PATH" is specified here then the PATH from the client environment is
# going to be used for looking up a relative `--server-path`.
# Example: pass_environment = ["PATH", "LD_LIBRARY_PATH"]
pass_environment = []

# milliseconds the language server must be quiet before lspmux answers owed
# diagnostics itself
#
# a client is owed diagnostics for a document after it opens, changes or saves
# it. servers like haskell-language-server only publish diagnostics when they
# change, so a client re-sending unchanged text, or a clean file, would wait
# forever. once the server has no work done progress active and sent nothing
# (and was sent no document changes) for this long, lspmux sends each owed
# client the diagnostics the server last published for its text, or an empty
# list, at the client's document version.
settle_ms = 1500

# settings lspmux owns for a server, keyed by the `--server-path` value
#
# the value is a JSON file with the settings object, its top-level keys are
# sections, e.g. `{"haskell": {...}}`. a leading "~/" expands to $HOME. with an
# entry for a server lspmux sends the object as the server's
# `initializationOptions` and in one `workspace/didChangeConfiguration` after
# initialization, answers `workspace/configuration` from it (a `section` is a
# dotted path, missing ones are `null`) and drops the clients'
# `workspace/didChangeConfiguration`. a file which can't be loaded is logged
# and the server is treated as unconfigured: clients answer as usual.
#
# there are no entries by default.
[server_settings]
# "haskell-language-server-wrapper" = "~/.config/lspmux/settings/hls.json"
```


## Sharing a server between different clients

Clients sharing a server instance (say an editor and a headless agent) may
support different parts of the protocol. lspmux keeps every client's
`initialize` capabilities and `clientInfo` (shown by `lspmux status`) and:

- initializes the server with the first client's parameters, but always
  declares `workspace.configuration`,
  `workspace.didChangeWatchedFiles.dynamicRegistration`,
  `window.workDoneProgress` and
  `textDocument.publishDiagnostics.versionSupport`, lspmux handles these
  itself
- forwards `client/registerCapability` only to clients declaring
  `dynamicRegistration` for the method (unknown methods go to everyone),
  including registrations replayed to clients connecting later, and
  unregistrations only to clients which got the registration
- forwards `workspace/didChangeWatchedFiles` only from one watcher, the
  earliest connected client which got the watched files registration, the
  next one takes over when it disconnects; without a watcher every client's
  notifications are forwarded
- answers `workspace/*/refresh` server requests itself and forwards them only
  to clients declaring `refreshSupport`
- replays active work done progress to clients connecting to a running server;
  when nothing is in progress they get a progress on the `lspmux/attach` token
  which ends right away, so clients waiting for the server to finish loading
  don't have to time out
- answers owed diagnostics once the server settled, see `settle_ms`


## Other LSP servers

By default `lspmux` uses a `rust-analyzer` binary found in its `$PATH` as the
server. This can be overridden using the `--server-path` cli option with the
client subcommand or `LSPMUX_SERVER` environment variable. You can usually
configure one of these in your editor configuration. If both are specified the
cli option overrides the environment variable.

For example with `coc-clangd` in CoC for neovim add to
`~/.config/nvim/coc-settings.json`:

```json
{
    "clangd.path": "/home/user/.cargo/bin/lspmux",
    "clangd.arguments": ["client", "--server-path", "/usr/bin/clangd"]
}
```

Or to set a custom path for `rust-analyzer` with `coc-rust-analyzer` add to
`~/.config/nvim/coc-settings.json`:

```json
{
    "rust-analyzer.server.path": "/home/user/.cargo/bin/lspmux",
    "rust-analyzer.server.extraEnv": { "LSPMUX_SERVER": "/custom/path/rust-analyzer" }
}
```

If your editor configuration or plugin doesn't allow to add either you can
instead create a wrapper shell script and set it as the server path directly.
For example if `coc-clangd` didn't allow to pass additional arguments you'd
need a script like `/usr/local/bin/clangd-proxy`:

```sh
#!/bin/sh
exec /home/user/.cargo/bin/lspmux client --server-path /usr/bin/clangd $@
```

And configure the editor to use the wrapper script in
`~/.config/nvim/coc-settings.json`:

```json
{
    "clangd.path": "/usr/local/bin/clangd-proxy"
}
```
