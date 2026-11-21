# Terminal UI

`reagent` with no task opens the terminal UI.
This page lists the environment variables that change how it draws.

## Draw size

| Variable | Value | Meaning |
|---|---|---|
| `REAGENT_MAX_COLS` | a whole number from 1 to 65535 | the most columns the UI draws in |
| `REAGENT_MAX_ROWS` | a whole number from 1 to 65535 | the most rows the UI draws in |

The UI draws in the smaller of the terminal's own size and the limit, from the top left corner.
An unset or empty variable sets no limit on that side, and any other value stops the start with an error that names the variable.

A limit is for a viewer that shows the terminal in a box narrower than the terminal itself, such as a board that shows a headless terminal of 200 columns in a box of 95.
Without one, each row of the UI is wider than the box, and the viewer wraps it onto two or three lines.

```sh
REAGENT_MAX_COLS=95 reagent
```

The UI still follows a resize of the terminal below the limit.
