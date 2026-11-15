# Plugin events

A plugin listens to the events of a run with `reagent.on(name, fn)`.
The function runs once for each event the run sends.
The payload of each event is one object, and it is passed to the function as its only argument.

```js
import { appendFile } from "node:fs/promises";

export default function (reagent) {
  reagent.on("usage", async (e) => {
    if (e.cost !== null) await appendFile("cost.log", `${e.model}: ${e.cost} USD\n`);
  });
}
```

`console` and `process` are not globals in a plugin: a plugin that names them gets `console is not defined` or `process is not defined`.
Import what you need from the node modules, such as `import process from "node:process"`, and write output to a file or a panel (`reagent.ui.panel`).
A listener that throws leaves one note, `plugin <name>: event <event> threw: <message>`, which the run prints on stderr or shows its clients; a listener that catches its own error leaves none.

A listener runs on the plugin thread, so it costs the run nothing but the payload.
A run whose plugins listen on no event builds no payload at all.

A name that is not in this page is an error at load time, and the error lists the names this page has.

## The events

| Name | When it fires | Payload keys |
|---|---|---|
| `run_start` | A run starts | `version`, `hooks` |
| `turn_start` | A turn starts | none |
| `turn_end` | A turn ends | `reason` |
| `tool_end` | A tool call has its result | `tool`, `isError` |
| `run_end` | A run ends | `reason` |
| `usage` | A model reply finishes | see below |
| `waiting` | The run starts to wait for a person | `kind`, `call`, `tool`, `session` |
| `waited` | That wait ends | `kind`, `call`, `how`, `session` |

`reason` is one of `end_turn`, `cancelled`, `parked`, `stopped`, `max_turns` and `error`.
It is the same word the `--json` output writes for the end of a turn or a run.
`tool` is the tool's own name, and `isError` says whether its result is an error.
`version` is the version of the reagent binary that runs, as `reagent --version` prints it.
`hooks` says what the `tool.call` hook covers in this build: `hooks.codemodeInner` is `true` when the calls a `codemode` script makes go through the hook. A build without the key lets a script's calls skip it, so a gate that must see every call should block `codemode` unless `hooks.codemodeInner` is `true`.

## The `waiting` and `waited` events

A run sends `waiting` when it starts to wait for a person, and one `waited` when that wait ends, with the same `kind` and the same `call`.
The two events say which tool call holds the run, so a coordinator of agents can tell a worker that waits from one that works.

A run sends both only when a plugin listens on them.

| Key | Type | Meaning |
|---|---|---|
| `kind` | string | `question` for an `ask` question shown to the clients, `approval` for a permission prompt |
| `call` | string | the id of the tool call that waits, the same id the `--json` output writes for it |
| `tool` | string | the tool's own name, `ask` for a question |
| `session` | string or null | the id of the run's session, null for a run that keeps no session |
| `how` | string | what ended the wait: `answered`, `denied`, `cancelled`, `timeout` or `parked` |

`how` is `answered` for a question a client answered and an approval a client granted, `denied` for an approval a client refused, `cancelled` for a wait the run's cancellation ended, `parked` for an `ask` that parked because its last client left, and `timeout` for a wait that ran out of time.
No wait has a timer today, so `timeout` comes from a later version.
`denied` also covers a prompt that reached no client at all.

## The `usage` event

It fires once for each finished model reply, after the reply ends and before its tool calls run.
The payload is the token use of that reply, with the names the `--json` output writes for it.
A reply whose provider reported no token use at all (every count 0 and no cost) sends no event.

The events of a run wait in a queue for the plugin thread.
When a listener is so slow that the queue is full, a new event is dropped and the run adds one note that counts the dropped events.
A reader that must count every reply keeps its listener short, such as one append to a file.

| Key | Type | Meaning |
|---|---|---|
| `id` | string | 32 lowercase hex characters, one per reply |
| `at` | number | milliseconds since the Unix epoch, when the reply ended |
| `session` | string or null | the id of the run's session, null for a run that keeps no session |
| `parentSession` | null | the session of the parent run, null until reagent has subagents |
| `provider` | string or null | the reagent provider id of the run's config, null for the bare wire |
| `preset` | string or null | the built-in provider the provider is made from, null when it has none |
| `model` | string or null | the model the request named |
| `input` | number | uncached input tokens |
| `output` | number | output tokens, `reasoning` included |
| `cacheRead` | number | input tokens read from the provider's cache |
| `cacheWrite` | number | input tokens written to the provider's cache |
| `reasoning` | number | reasoning tokens, 0 when the provider does not report them |
| `cost` | number or null | USD, the same cost the `--json` usage line shows, null when no price is known |
| `cwd` | string | the absolute path of the folder the run works in |

Every key is always there, and a key with no value is `null`.

`input` is the uncached input only.
It does not include `cacheRead` or `cacheWrite`, so the total input of a reply is `input + cacheRead + cacheWrite`.

`reasoning` is a part of `output`, not extra tokens.
The total of the reply is `input + cacheRead + cacheWrite + output`.

`id` is unique per reply.
A reader that counts each reply once keeps the ids it has seen, since the event can fire again for the same reply only when the plugin host replays it, and then with the same `id`.

`provider` is the id the user wrote in their config, and the user names it freely.
`preset` is the built-in kind that id is made from, so a reader can map a reply to a model maker without reading the user's config.
The preset ids are `anthropic`, `chatgpt`, `commandcode`, `deepseek`, `freeinference`, `mistral`, `mistral-vibe`, `openai`, `opencode-go`, `openrouter`, `radius` and `synthetic`.
A provider the run switched to mid-run reports `preset` as null, because the run reads the preset of the provider it started on.

`parentSession` is null until reagent has subagents.
It is in the payload now so that its shape does not change when they come.

A later version may add keys to this payload.
A reader ignores the keys it does not know.

## Gateway events

Two more events reach gateway plugins, and only them: a plugin whose
`package.json` sets `"reagent": { "role": "gateway" }`, loaded by
`reagent gateway`. A normal run never fires them, so a normal plugin that
listens on them hears nothing.

`question_parked` fires once for each parked question the gateway has not
announced yet.

| key | type | meaning |
|---|---|---|
| `qid` | string | the id of the question in its journal |
| `session` | string | the session that waits for the answer |
| `questions` | array | the questions of the call, as the model asked them |
| `at` | number | when the run parked it, in milliseconds since the epoch |
| `deadline` | number or `null` | when it expires, in milliseconds since the epoch; `null` when it never does |
| `token` | string | 32 lowercase hex digits, the secret `reagent.gateway.answer` wants |

`question_closed` fires once for each announced question that is no longer
open.

| key | type | meaning |
|---|---|---|
| `qid` | string | the id of the question |
| `how` | string | `"answered"` or `"expired"` |
| `by` | string or `null` | who closed it, such as `"cli"`, `"deadline"` or `"gateway:<plugin>:<by>"`; `null` when no record names it |

The answers go back through `reagent.gateway.answer`, which only a gateway
plugin has. See `docs/gateway.md`.

## Requiring a plugin

A plugin that guards a run, such as a `tool.call` gate, must not be skipped in silence when it fails to load.
`reagent --require-plugin <name>` names a plugin the run must load, by the `name` in its `package.json`; the flag may be given more than once.
When a named plugin did not load (it is missing, its manifest is wrong or its code throws at load), the run prints `reagent: required plugin <name> did not load; the run stops` and exits with code 1 before its first model request.
`--require-plugin` with `--no-plugins` is an error.

## The `tool.call` hook

`reagent.hook("tool.call", fn)` installs a layer that sees every tool call before it runs and decides what to do with it.
The handler gets one object and returns nothing, `{ block }` or `{ args }`.

```js
export default function (reagent) {
  reagent.hook("tool.call", ({ tool, args }) => {
    if (tool === "bash" && String(args.command).startsWith("rm -rf")) {
      return { block: "the gate refuses to remove files" };
    }
    return undefined;
  }, { timeoutMs: 200, onTimeout: "block" });
}
```

| Key | Type | Meaning |
|---|---|---|
| `tool` | string | the tool's own name |
| `args` | object | the arguments after the tool's own repair, so `"  5 "` is already `5` |
| `cwd` | string | the absolute path of the folder the run works in |
| `session` | string or null | the id of the run's session, null for a run that keeps no session |

A `tool.result` layer gets the same `cwd` and `session` keys.

The handler runs before the permission rules, on the tool's own name and on the repaired arguments, so a blocked call never reaches a rule and never runs.
A call that a `codemode` script makes goes through the same layers with the inner tool's own name and arguments, so a script cannot get past a gate. The `codemode` call itself is gated too, with `tool: "codemode"` and `args: { code }`.
A layer that returns nothing keeps the call as it is.
`{ args }` replaces the arguments for the call and for every layer after it.
`{ block }` is a reason a person reads: the call does not run, and the model gets an error result with that reason as its text.
`{ block: false }` is not a block.

A plugin may install a layer only when its `package.json` lists the hook:

```json
{ "reagent": { "permissions": { "hooks": ["tool.call"] } } }
```

The list is a list of hook names, and a name that is not a hook is an error at load time that names it.
A plugin that starts sessions of its own wants the `sessions` permission, which [plugin-sessions.md](plugin-sessions.md) describes.
Without it, `reagent.hook("tool.call", fn)` throws `plugin <name> has no hooks permission for tool.call`.
`tool.result` and `turn.end` need no permission.

The third argument is optional:

| Key | Type | Default | Meaning |
|---|---|---|---|
| `timeoutMs` | number | `5000` | how long the layer has to answer, 1 to 120000 |
| `onTimeout` | string | `"keep"` | `"block"` or `"keep"` |

A layer that does not settle within `timeoutMs` gives its `onTimeout`: `"keep"` keeps the call as it is, and `"block"` blocks it with the reason `plugin <name>: tool.call hook timed out after <ms> ms`.
A layer that throws or rejects gives the same outcome, with the reason `plugin <name>: tool.call hook failed: <message>`.
A value that is not one of the above throws at `reagent.hook`.

Each layer has its own limit, and a chain of layers is bounded by the sum of them: no layer is cut short by the limit of another.
A layer may wait on a promise for its whole `timeoutMs`, for example while it asks a coordinator. Each piece of its JS that runs without a pause still stops after 5 s, as every plugin call does. The plugin thread serves one call at a time, so a long wait also delays the plugin's other hooks and events.
