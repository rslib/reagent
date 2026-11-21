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
| `initialize` | Answers protocol version 1, `loadSession`, `sessionCapabilities` for `list`, `resume`, `close` and `delete`, `promptCapabilities` with `image` and `embeddedContext`, and an empty `authMethods`. |
| `authenticate` | Answers `{}`. Agent token auth is configured in the config files, not here. |
| `session/new` | Starts a worker in `cwd` and answers the reagent session id, its `configOptions` and its `modes`. |
| `session/load` | Joins the live worker of the session, or starts one with `--resume`, replays the conversation as `session/update` notifications, and then answers its `configOptions` and its `modes`. |
| `session/resume` | The same as `session/load` without the replay, for an editor that keeps its own copy of the conversation. |
| `session/list` | Answers the sessions of one folder, or of every folder, newest first. See [Sessions](#sessions). |
| `session/close` | Ends the task of the session and detaches it, so its worker stops when no client is left. A prompt that waits answers `cancelled` first. |
| `session/delete` | Removes the folder of one session that no worker serves. |
| `session/set_config_option` | Sets one of the options below and answers every option as it stands after the change. A config id this agent has no option for, or a value outside the options of a select, is `-32602`. |
| `session/set_mode` | Sets the permission mode and answers `{}`. A mode this agent does not know is `-32602`. |
| `session/prompt` | Sends the prompt to the session and answers `stopReason` when the run ends. |
| `session/cancel` | Stops the open turn. The prompt then answers `cancelled`. |
| `$/cancel_request` | Stops the prompt its `requestId` names, the way `session/cancel` does. An id that names no prompt this agent waits for is ignored. |

`configOptions` holds three options, each of type `select`:

- `model`, of category `model`: its `currentValue` is the `provider/model` the
  worker runs, and its options are the models the model sheet of the terminal
  UI lists for the session's folder: the recent ones, then the models of each
  provider that can sign in. An editor may set a model that is not listed.
- `thought_level`, of category `thought_level`: its `currentValue` is the
  reasoning level the worker sends, and `off` for a session that sends none.
  Its options are the levels the current model takes, from the models cache,
  and `off`, `low`, `medium`, `high` for a model whose own levels the cache
  does not name. A model the sheet lists both as recent and under its provider
  takes the levels the provider names. The value in force is always one of
  them.
- `mode`, of category `mode`: its `currentValue` is the permission mode, and
  its options are `default`, `accept-edits`, `bypass` and `auto`.

`modes` names the mode in force as `currentModeId` and the same four as
`availableModes`. A mode the run does not enable, `bypass`, is refused by the
run: the session keeps its mode, the editor gets `config_option_update` and
`current_mode_update` naming the mode it kept, and the reason shows as an
`agent_thought_chunk`. The answer to a set of `bypass` names the mode the session
kept until the worker confirms the mode in its `settings` notice.

The `initialize` answer of a worker names the mode in force as `mode`, so a client
that loads or joins a live session starts from it. A worker older than the field
names none, and the session then starts from `default`.

A provider error that reagent retries is a `notice` with severity `warning`, or an
`agent_thought_chunk` such as `retry 1: provider status 503: ...` for an editor
that advertises no notices, so a wait does not look stuck. An editor that
advertises notices also takes the error of a failing run as a `notice` with
severity `error`, while the prompt itself answers `-32603` with the provider's
message. A provider that asks to wait longer than a minute, such as a usage limit
that resets in hours, is not retried: the prompt fails at once with its message.

A prompt whose text starts with `/<name>` of a command a plugin of the session
offers runs that command through the worker's command request, with the rest of
the text as its arguments, and never reaches the model. What the command prints
is the reply. When the command starts no run, the prompt answers `end_turn` once
the command is done, or `cancelled` when the editor cancelled meanwhile, and a
refused command answers the prompt with the worker's error; when it queues a message, the prompt answers when that run
ends. A skill is not a plugin command: `/name` of a skill goes as text, which is
how the worker expands it.

The prompt takes `text`, `resource_link` (sent as `[name](uri)`), `resource`
with text (sent as a fenced block headed by its uri), an `image`, and a
`resource` whose blob has an image media type. Each image is written to a file
of the `images` folder of the session, in the state folder and never in the work
tree, named by a hash of its bytes, and the prompt names that file with an
`@path` word of its own, which the worker attaches as it attaches such a word of
the terminal UI. An image of more than 20 MiB, an `audio`, and a `resource` with
a blob that is not an image are refused with `-32602`. The base64 text of an
image is decoded here, and text that is not base64 is refused with `-32602`. The
worker decides what it attaches: a payload it cannot read as an image is sent and
not attached.

Updates of a session:

- `user_message_chunk`, `agent_message_chunk` and `agent_thought_chunk` for the
  text of the conversation. Each chunk carries a `messageId`: chunks of one
  user message or one turn of the agent share it, and the next message or turn
  names another.
- `tool_call` when a call starts, with `status: pending`, the programmatic
  `name` of the tool, and the kind of work:
  `read` for `read`, `edit` for `edit` and `write`, `execute` for `bash`,
  `search` for `grep` and `find`, `fetch` for `fetch` and `web`, and `other`
  otherwise. No reagent tool deletes, moves or only reasons, so this agent sends
  no `delete`, `move` or `think` kind.
- `tool_call_update` with `status: in_progress`, the parsed `rawInput`, the file
  the call works in, and a title such as `Read src/a.rs` or `$ cargo test`.
- `tool_call_update` with `status: completed` or `failed`, the output of the
  call as its `content`, and the first bytes of that text as `rawOutput`. An
  `edit` and a `write` also carry a `diff` content item.
- `available_commands_update` with the skills the session found and the commands
  its JS plugins registered, in name order. It is sent when a session opens and
  when the commands of its plugins change, and it replaces the list the editor
  holds. A command with arguments carries an `input` hint.
- `session_info_update` with the `title` of the session and `updatedAt`, the
  time of the update as ISO 8601. The title is the first user text cut as the
  session list cuts it: control characters become spaces, at most 60 characters,
  and the `@"path"` words that attach an image are left out, so a prompt of only
  an image names no title yet. It is sent once, when that prompt arrives. A
  session that is loaded or resumed sends the title of its first user text when
  it opens, and a later prompt does not rename it.
- `usage_update` after each reply with the tokens the request used, `used`,
  the context window of the model as `size`, and `cost` in USD when the provider
  names a price. A model whose window the run does not know reports the budget
  the run keeps the session under instead.
- `config_option_update` with every option, and `current_mode_update` with the
  mode, when another client changes the settings of the session. A change this
  agent made itself is in the answer to the request that made it, so it sends
  no update of its own.

The prompt answers one of the v1 stop reasons:

- `end_turn` when the model gave its reply.
- `max_tokens` when the output limit of the model stopped the reply.
- `max_turn_requests` when the turn limit of the session ended the run.
- `refusal` when the model refused to answer.
- `cancelled` when `session/cancel` or `$/cancel_request` stopped the turn.

A run that ends at its turn limit, or one the model stopped at its token cap, may
leave the task unfinished, and the stop reason says so.

## Sessions

`session/list` takes an optional absolute `cwd` and an optional `cursor`, and
answers `sessions` and `nextCursor`. A session it lists carries the `sessionId`,
the `cwd` it ran in, its `title` (the first message, cut, or `null` for a session
with none) and the `updatedAt` of its last record, as ISO 8601 in UTC, such as
`2026-10-27T14:03:11Z`. The list is newest first, and a page holds 50 sessions: a
cursor of the page before is passed back in, and `nextCursor` is `null` on the
last page.

Without `cwd` the list covers every folder of the state folder, and each session
names the folder its journal records. With `cwd`, each session names the `cwd`
the request gave, which is the folder the editor knows. A `cwd` that is not an
existing folder, or a cursor this agent did not write, is `-32602`.

`session/close` takes a `sessionId` and answers `{}`. It cancels the work of the
session as `session/cancel` does, answers a prompt that waits `cancelled`, and
lets go of the worker: a worker with no client left stops after its linger. A
closed session takes no prompt, which is `-32002`, and a second close of it
answers the same `{}`.

`session/delete` takes a `sessionId` and answers `{}`. It refuses a session whose
worker still runs, with `-32602`, so the editor closes it first. It removes the
folder of that one session, and only inside the sessions root of the state
folder: a session folder that is a link, or inside a folder that is a link or
whose real path is outside the root, is refused with `-32602`. A session that is
already gone is deleted with the same `{}`.

`$/cancel_request` takes a `requestId`, which is the id of an earlier request of
the editor. It stops a `session/prompt` the agent still runs and answers
`cancelled`, as `session/cancel` does. A request of another kind that the agent
still works on, such as a `session/list` that has not finished, is answered
`-32800`, and an id that names no request at all is ignored. The number 7 and the
string "7" are two ids. The agent reads the next line while a `session/list` or a
`session/delete` runs. A `session/delete` is not cancellable: a `$/cancel_request`
for it is ignored, and the delete answers its own result.

A line over 28 MiB, which an image of the largest size in base64 text and its
JSON fit in, is answered `-32600` with a `null` id, since its id is not read.

When a run ends, or another client answers an approval or a question first, a
`session/request_permission` or an `elicitation/create` the editor still holds
is taken back: the agent sends `$/cancel_request` with the `requestId` of that
request.

## Approvals and questions

A tool call that needs a person becomes `session/request_permission` with three
options:

- `Allow once` runs that call.
- `Allow always` runs it and saves the rule, or allows it for the session when
  the worker offers no rule to save.
- `Reject` refuses that call.

An answer of `cancelled`, or an option this agent does not know, skips the
call: it does not run, and the model is told to go on without it. A second client that answers the same worker request first makes this
request's own answer a no-op.

The `ask` tool is shown as a permission request when the editor advertises no
elicitation form and the call puts exactly one single-choice question with
options: one `Allow once` option per label, and the label picked is the answer.
Any other question set is then answered with no answer and a note in the
transcript that names the question, so the editor does not block the run on a
card it cannot draw.

An editor whose `clientCapabilities` name `elicitation` with a `form` takes the
whole question set as one `elicitation/create` request instead, in `form` mode:
one field per question, named `q0`, `q1`, and so on, and the `message` is the
text of the first question. A single-choice question is a `string` field with
one `oneOf` entry per label, a multiple-choice question an `array` of them, and a
question with no option a `string` field that takes any text. The form is
answered `accept` with one value per field, and the answer of each question is
the label or labels picked. A declined or a failed form is the answer of no
question, which the worker reads as a skip.

## What it does not do

- Audio and blobs that are not images: no prompt block of either kind is taken.
- `plan`: none is sent.
- `fs/read_text_file`, `fs/write_text_file` and the `terminal/*` methods: this
  agent works on the file system of its own workers instead.
- `logout`: no such method, so `-32601`.

## The terminal UI on the same session

A worker started by the editor is a worker like any other: `reagent sessions`
lists it, and `reagent -r <session>` attaches to it. The TUI and the editor then
see the same run, and either can answer an approval first.
