# reagent-task

A plugin that gives the model one tool, `task`.
`task` hands a self-contained job to a helper session and answers what that helper said.
The helper can only read: it has the `read`, `find` and `grep` tools, and no other one.
The helper sees none of the conversation that asked, so the prompt of the call must hold the whole job.
A `task` call returns the last message of the helper, so a helper puts every finding it needs to report there.

## Install

Copy the folder into the `plugins` folder of the config, or link it there:

```sh
cp -r plugins/task ~/.config/reagent/plugins/task
ln -s "$PWD/plugins/task" ~/.config/reagent/plugins/task
```

The folder is `<config>/plugins/<folder>/package.json`, where `<config>` is the config folder of reagent.
That folder is `$XDG_CONFIG_HOME/reagent`, or `~/.config/reagent` when `XDG_CONFIG_HOME` is not set.
A link of the whole folder works, because the plugin folder itself is resolved before it loads.
A link at a file inside the folder does not work, because the entry must stay inside the plugin folder.
A run says `entry <name> leaves the plugin folder` for an entry that does not.

## The two variables

The helper runs on the provider, the model and the folder of the run that asked, unless a variable says otherwise.
The plugin reads two variables, which its `package.json` lists under `permissions.env`.

| Variable | Meaning |
|---|---|
| `REAGENT_TASK_PROVIDER` | the provider a helper starts on, as `--provider` names it, such as `fake:replies.json` |
| `REAGENT_TASK_MODEL` | the model a helper starts on |

A variable that is unset or empty keeps what the run uses.

## Limits

A run starts one helper at a time: the plugin thread serves one `task` call at a time, so a second call waits for the first to answer.
A helper cannot start helpers of its own, because a session at depth one starts no session.
A `task` call waits 30 minutes at most for its helper.
Esc stops the helper: a cancel of the run cancels the call, and the call cancels the session it started.
