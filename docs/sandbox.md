# Sandbox

The sandbox fences a bash command and every child it starts.
It is on with the `[sandbox]` table of the global `config.toml`:

```toml
[sandbox]
mode = "on"                       # off (the default), auto or on
network = ["github.com:443"]      # false (the default), true, or a host list
write = ["~/.cache"]              # folders a command may write
```

`mode = "auto"` sandboxes bash when the sandbox can start, and runs it directly when it cannot.
With a host list in `network`, `auto` acts as `on`: a command that cannot get the sandbox is refused, so it never gets the whole network.
`mode = "on"` sandboxes bash or refuses to run it.

The sandbox is applied by the hidden helper `reagent --sandbox-exec`, which sandboxes itself and then becomes the command, so every child of the command is fenced too.
macOS uses a Seatbelt profile.
Linux uses Landlock, plus a seccomp filter that refuses every socket but a local one when the network is off.

## The `[sandbox]` keys

| Key | Values | Default |
|---|---|---|
| `mode` | `"off"`, `"auto"`, `"on"` | `"off"` |
| `network` | `false`, `true`, or a list of `"host:port"` | `false` |
| `write` | a list of folders | none |

A command may read anywhere and write only in the working folder, the temp folder and the `write` folders.
The `[sandbox]` table may be set only in the global file or a profile.
A profile replaces a value the global file set.

## The network

`network = false` gives a command no network at all.

`network = true` gives it the whole network.

`network = ["host:port", ...]` gives it those hosts and nothing else.
Each entry is a host and a port, for example `"devbox.example.net:22"`, `"github.com:443"` or `"127.0.0.1:8080"`.
Write an IPv6 host in brackets: `"[::1]:8080"`.
Each entry is checked when the config loads, and a wrong entry is a config error that names it.
An empty list is no network.

Only allow entries exist.
There is no `!host:port` form and no deny list.

### How a host list works

No OS sandbox can allow a list of remote hosts.
Seatbelt names the host of a `remote ip` filter only as `*` or `localhost`, and Landlock filters TCP by port and never by host.

So a run with a host list starts one proxy of its own on a random port of `127.0.0.1`.
The proxy is the only way out: the sandbox lets a command reach the proxy port and the listed local ports, and no other address.

The proxy speaks three protocols:

- HTTP `CONNECT host:port`, for `curl`, `git` and most HTTPS clients.
- Plain HTTP with an absolute URI, which the proxy forwards in origin form.
- SOCKS5 with no auth and the `CONNECT` command, for `ssh` and other tools.

A target is allowed only when its `host:port` is one of the entries.
The host is compared without case.
An IP literal is compared as an address.

A target that is not in the list is refused: HTTP answers `403 Forbidden` with the text `reagent sandbox: <host:port> is not in sandbox.network`, and SOCKS5 answers `0x02`.
The run prints nothing about it.

The proxy resolves a name itself, at the moment of the connection, so a DNS change is followed without a restart.
The proxy ends with the run.

### The variables a command gets

The helper sets these variables for the command, also with `--env-clear`:

| Variable | Value |
|---|---|
| `HTTP_PROXY`, `HTTPS_PROXY`, `http_proxy`, `https_proxy` | `http://127.0.0.1:PORT` |
| `ALL_PROXY`, `all_proxy` | `socks5h://127.0.0.1:PORT` |
| `REAGENT_PROXY` | `127.0.0.1:PORT` |

It also removes `NO_PROXY` and `no_proxy`, so no client skips the proxy for a host.

A program that ignores these variables is blocked, because the sandbox lets it reach nothing but the proxy port.
That is what "fail closed" means here: a command with no proxy support in its tools gets no network, not the whole network.

`ssh` does not read `ALL_PROXY`, so name the proxy in its own setting:

```bash
ssh -o ProxyCommand='nc -X 5 -x $REAGENT_PROXY %h %p' devbox
```

### Linux

Landlock filters TCP by port and never by host, so on Linux a host list is a network namespace, not a firewall rule.

The helper first puts the command in a user and a network namespace of its own, where the only interface is loopback.
It maps the user's own ids into that namespace, so the files the command writes keep their owner.
Inside the namespace it listens on the proxy's port and on each listed local port.
One bridge process, forked before the command starts and never exec'ing anything, joins each of those ports byte for byte to the run's proxy.
The proxy listens on a unix socket in a new folder of mode 0700 under the temp folder, which is removed when the run ends.

So the command gets Landlock for the file system only: a socket of its namespace reaches nothing but the bridge, so no seccomp filter and no Landlock network rule is added.
A target that is not in the list is still refused by the proxy, as on macOS.
A listed local port is reached directly, through its own listener of the bridge.
The bridge dies with the command and leaves nothing behind.

The price is one more process while a command with a list runs.
A `network = true` or `network = false` run is unchanged: no namespace, no bridge, the same helper arguments.

Where a user namespace cannot be made, some containers turn them off, a list is no network at all.
The run probes once at start, in a child of its own, and when the probe fails it prints this one line on stderr:

```
reagent: sandbox.network host lists need user namespaces, which are off here; commands get no network
```

It never falls back to the whole network: a step of the namespace, or of the bridge, that fails runs the command under the `network = false` fence.

### Rule precedence is not built

With a host list the sandbox limits where any process can connect, so a broad rule is safe.
With `network = ["devbox.example.net:22"]` you may allow `Bash(ssh:*)`: the sandbox keeps `ssh` to the listed hosts, whatever the command text is.
That is why there is no `except` form: the sandbox, not the rule text, is what limits a connection.

## What rules stop and what only the sandbox stops

A permission rule sees only the command text the model types, the way `rm -rf target` is written.
It matches that text and nothing else.

A child process or a script is stopped only by the sandbox.
`python3 -c "..."`, `make`, `npm test` and a shell function all run code that no rule can read.
The sandbox fences that code by what the process may touch: its folders, its network, its process group.

So write a rule for the command you mean to see, and let the sandbox hold the rest.
