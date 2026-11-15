# MCP servers

An MCP server gives the model tools of its own.
The servers are named in `mcp.toml` in the config folder, one `[servers.<name>]`
table each; `reagent mcp add|remove|list` writes that file, and
`reagent mcp login` signs an HTTP server in.

A run starts every server whose `enabled` is true, and `--mcp` picks others:

```bash
reagent -p "list the issues" --mcp +github   # add a server of mcp.toml
reagent -p "list the issues" --mcp -slack    # remove one
reagent -p "list the issues" --no-mcp        # start none
```

## `--mcp-spec`

`--mcp-spec <json>` starts one server that `mcp.toml` does not name, and it may
be given more than once.
The JSON is the stdio shape an editor sends in an ACP `session/new` request:

```bash
reagent -p "read the file" \
  --mcp-spec '{"name":"fs","command":"/usr/bin/npx","args":["-y","server-filesystem"],"env":[{"name":"ROOT","value":"/tmp"}]}'
```

The http shape takes a `url` and its `headers`:

```bash
reagent -p "open the issue" \
  --mcp-spec '{"type":"http","name":"issues","url":"https://example.com/mcp","headers":[{"name":"Authorization","value":"Bearer ${TOKEN}"}]}'
```

The server takes the defaults of a `mcp.toml` entry: a 60 second timeout, on,
and its tools reached through `codemode`.
`command` must be an absolute path.
An `env` or a `headers` value goes to the server as written, and a `${VAR}`
reference in it is the value of that variable, the same way the file writes it.
An `sse` server is refused, since there is no transport for it.

A spec never replaces a configured server: a `name` that `mcp.toml` also names
is an error that names it.
Bad JSON, a missing `name`, `command` or `url`, and a name that `mcp.toml` could
not hold (`a-z`, `0-9`, `-`, `_`) are errors too.
No error, log line or debug output holds an `env` or a `header` value.
`--no-mcp` starts no server at all, so it drops a spec server too.
