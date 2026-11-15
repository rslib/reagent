# `reagent acp`

`reagent acp` speaks [Agent Client Protocol](https://agentclientprotocol.com/agent-client-protocol)
version 1 on stdin and stdout, so an editor runs reagent as its agent. One
process serves every session of the editor. Each session is one reagent worker,
which this process starts or joins as a socket client, so the terminal UI can
join the same session while the editor drives it.

The command is part of the default build. A build without the `acp` feature
answers `reagent: this build has no acp` and exits 2.

```bash
reagent acp                        # what the editor runs
reagent acp --model opus --profile work
```

Every flag after `acp` reaches each worker this process starts, so
`--model`, `--profile`, `--provider` and `--permission-mode` set the sessions of
the editor the same way they set a run of the terminal UI.

## Zed

Open the Agent Settings, go to the External Agents page, click `Add Agent` and
choose `Add Custom Agent`, then name the agent. Zed writes an `agent_servers`
entry:

```json
{
  "agent_servers": {
    "reagent": {
      "type": "custom",
      "command": "reagent",
      "args": ["acp"],
      "env": {}
    }
  }
}
```

The palette command `dev: open acp logs` shows the messages between Zed and the
agent. Zed forwards the MCP servers of its settings to the agent in `mcpServers`
of `session/new`, and this agent passes each of them to its worker as one
`--mcp-spec` flag, so a stdio server of the editor is a tool of the session.

## What version 1 supports here

| Method | What it does |
| --- | --- |
| `initialize` | Answers protocol version 1, `loadSession`, `fss` off, `terminal` off, `promptCapabilities` with `embeddedContext`, and an empty `authMethods`. |
| `authenticate` | Answers `{}`. Agent token auth is configured in the config files, not here. |
| `session/new` | Starts a worker in `cwd` and answers the reagent session id. |
| `session/load` | Joins the live worker of the session, or starts one with `--resume`, replays the conversation as `session/update` notifications, and then answers `{}`. |
| `session/prompt` | Sends the prompt to the session and answers `stopReason` when the run ends. |
| `session/cancel` | Stops the open turn. The prompt then answers `cancelled`. |

The prompt takes `text`, `resource_link` (sent as `[name](uri)`) and `resource`
with text (sent as a fenced block headed by its uri). An `image`, an `audio` or a
`resource` with a blob is refused with `-32602`, since `promptCapabilities`
advertises no image and no audio.

Updates of a session:

- `user_message_chunk`, `agent_message_chunk` and `agent_thought_chunk` for the
  text of the conversation.
- `tool_call` when a call starts, with `status: pending` and the kind of work:
  `read` for `read`, `edit` for `edit` and `write`, `execute` for `bash`,
  `search` for `grep` and `find`, `fetch` for `fetch` and `web`, and `other`
  otherwise.
- `tool_call_update` with `status: in_progress`, the parsed `rawInput`, the file
  the call works in, and a title such as `Read src/a.rs` or `$ cargo test`.
- `tool_call_update` with `status: completed` or `failed` and the output of the
  call. An `edit` and a `write` also carry a `diff` content item.

## Approvals and questions

A tool call that needs a person becomes `session/request_permission` with three
options:

- `Allow once` runs that call.
- `Allow always` runs it and saves the rule, or allows it for the session when
  the worker offers no rule to save.
- `Reject` refuses that call.

An answer of `cancelled`, or an option this agent does not know, counts as a
refusal. A second client that answers the same worker request first makes this
request's own answer a no-op.

The `ask` tool is shown as a permission request when it puts exactly one
single-choice question with options: one `Allow once` option per label, and the
label picked is the answer. Any other question set is answered with no answer
and a note in the transcript that names the question, so the editor does not
block the run on a card it cannot draw.

## What it does not do

- Images and audio: no prompt block of either kind is taken.
- `plan`, `mode`, `config option` and slash-command updates: none is sent and
  no mode change is taken.
- `fs/read_text_file`, `fs/write_text_file` and the `terminal/*` methods: this
  agent works on the file system of its own workers instead.
- `session/set_mode`, `session/set_config_option`, `session/list`,
  `session/resume`, `session/close`, `session/delete` and `logout`: no such
  method, so `-32601`.

## The terminal UI on the same session

A worker started by the editor is a worker like any other: `reagent sessions`
lists it, and `reagent -r <session>` attaches to it. The TUI and the editor then
see the same run, and either can answer an approval first.
