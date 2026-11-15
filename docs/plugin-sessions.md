# Plugin sessions

A plugin starts sessions of its own with `reagent.sessions`: each one is a worker of the same binary, with its own journal, its own folder and its own turn.
The plugin queues messages into it, steers it, cancels it and reads its runs.
This page says what each call takes, what it answers and what the session inherits from the run that started it.
The events a plugin listens to in its own run are in [plugin-events.md](plugin-events.md).

## The permission

A plugin starts a session only when its `package.json` grants the permission:

```json
{ "reagent": { "permissions": { "sessions": true } } }
```

A plugin without it has no `reagent.sessions` at all, so `typeof reagent.sessions` is `"undefined"`.
The permission is read when the plugin loads, and a run whose plugins ask for none of it starts no session host and no worker.

## The calls

Every call answers a Promise, and a call that cannot happen rejects with a reason a person reads.
A call of a session that belongs to another plugin rejects, so one plugin never reaches the session of another.

| Call | Argument | Answers |
|---|---|---|
| `start(spec)` | an object, below | the id of the session |
| `send(id, text)` | a session id and a text | nothing, once the worker has it queued |
| `steer(id, text)` | a session id and a text | nothing, once the worker has it |
| `cancel(id)` | a session id | nothing, once the worker has it |
| `result(id)` | a session id | `{ reason, text }` |
| `on(id, fn)` | a session id and a function | a function that removes the listener |
| `list()` | nothing | the ids of the plugin's open sessions, oldest first |

`send` queues the text behind the run that is open, so it is the first message of the next run.
`steer` gives the text to the turn that runs now.
`cancel` stops the turn that runs, and the run ends with `cancelled`.

### `start`

| Key | Type | Meaning |
|---|---|---|
| `task` | string | the first message of the session, and it must not be empty, and not `-`, which would read a message from a stdin a worker does not have |
| `cwd` | string | the folder it works in, the folder of the run when absent |
| `provider` | string | the provider it starts on, as `--provider` names it |
| `model` | string | the model it starts on |
| `profile` | string | the config profile it reads |

Only `task` is required.
A `provider` of `fake:replies.json` starts the session on that scripted provider, which is how a test keeps the session off the provider of the run: a session that reads the same scripted replies as the run it starts from would call the tool that started it again, for ever.

A plugin may have 8 sessions open at once, and a start whose worker is still coming takes a place of its own, so nine starts at once still leave the ninth out.
A ninth `start` rejects with `plugin <name> has 8 sessions open, the limit`, and a session whose worker stopped frees its place and leaves `list`.

### `result`

`reason` is `end_turn`, `cancelled`, `stopped`, `max_turns`, `error`, or `other` for any other ending of the worker.
`text` is the assistant text of the last turn of that run.

A call answers the end of the run that ends next, or the end of the last run when that one has ended.
The end of a run stays the answer of every `result` that comes after it, so two calls after the same end answer it twice instead of waiting for a run nobody starts.
A later `send` or `steer` that the worker takes starts a new run, and a `result` after it waits for the end of that one.

A session whose worker stopped answers its last run end at once.
A session that ended no run rejects with `session <id> is closed`, and never waits.

### `on`

The function runs once for each frame the session sends while the listener is on it, with one event as its only argument.
The frames of a session reach the plugin only from the moment a listener is added: the frames of a run that already ended are not kept, and are not replayed.
Put the listener on a session before the run whose events it wants, which is the run a `send` starts:

```js
const ends = [];
const id = await reagent.sessions.start({ task: "say it" });
await reagent.sessions.result(id);
reagent.sessions.on(id, (e) => { if (e.type === "run_end") ends.push(e.reason); });
await reagent.sessions.send(id, "say it again");
```

A listener runs on the plugin thread, and the plugin thread serves one call at a time.
A tool call that waits on the `result` of its own session holds that thread, so the events of that session reach a listener after the call ends.

## The events

| Name | Keys |
|---|---|
| `turn_start` | none |
| `text_delta` | `text` |
| `thinking_delta` | `text` |
| `tool_start` | `tool`, `call` |
| `tool_call_ready` | `call`, `arguments` |
| `tool_result` | `call`, `isError`, `error` |
| `turn_end` | none |
| `run_end` | `reason` |
| `user_message` | `text` |
| `error` | `message` |

`tool` is the tool's own name, `call` is the id of the tool call, `arguments` is its arguments as text, and `isError` says whether its result is an error.
`error` is the reason of a failed tool result, or the message of an error the worker met.
`reason` is the same word `result` answers.

## What a session inherits from its run

A session's worker starts with the flags of the run that started it, minus the task and the flags a worker cannot take.

The words that name the session of this run stay behind: `-c`, `--continue`, `-p`, `--print`, `-r`, `--resume`, `--fork` and `--no-session`, with their values.
The words that serve people stay behind too, because a worker already has its clients: `--json`, `--no-serve`, `--listen`, `--commands` and `--inline`.

Every other flag passes on, so a session runs on the same provider, the same model, the same profile, the same tool list (`--tools`), the same policy (`--policy`), the same MCP servers, the same limits and the same reasoning setting.
It reads the same config file as the run, and takes its sandbox from it.
The `cwd`, `provider`, `model` and `profile` of the `start` call come after the flags of the run, so the session's own words are the ones its worker reads.

A session starts with no history: its first message is its `task`.

## The end of the run

When the run that started the sessions ends, its clients leave every session, and a start that still waits for its worker is answered with `the run ended before the session started`.
A session whose client left keeps the turn it runs and then stops, the way a worker with no client and no run open stops.
A `result` that still waits is answered with `reason` `error` when its session is left.

## Nested sessions

A session's worker loads the same plugins as the run that started it, so a plugin that starts a session would start one there too, and that one another, with no end.
Each worker a session starts gets the variable `REAGENT_SESSION_DEPTH` set to its run's depth plus one, and a run with no such variable is depth 0.

A run of depth 1 or more starts no session at all.
Its plugins load and work as they do in any run, so a `tool.call` gate still guards the tools of a child, and every `reagent.sessions` call rejects with `a session cannot start sessions`.
The flag `--no-plugins` is not passed to a child, because a child needs the plugins it inherits.

## A build without the terminal UI

Sessions need the worker of the terminal UI, and a build without the `tui` feature (`cargo build -p reagent --no-default-features`) has none.
A plugin that asks for the permission still loads and still has `reagent.sessions`, and every call rejects with `this build has no sessions`.

## An example

A plugin that hands a task to a session of its own and answers what it said:

```js
export default function (reagent) {
  reagent.registerTool({
    name: "task",
    description: "Hands a task to a session and answers what it said.",
    parameters: {
      type: "object",
      properties: { prompt: { type: "string" } },
      required: ["prompt"],
    },
    run: async (args) => {
      const id = await reagent.sessions.start({ task: String(args.prompt) });
      const { text } = await reagent.sessions.result(id);
      return text;
    },
  });
}
```

The session runs on the provider, the model and the folder of the run, so a `task` call that names no folder works where the plugin runs.
