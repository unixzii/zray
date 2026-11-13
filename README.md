# zray

Interactive history search for zsh. Press <kbd>Up</kbd> at the prompt and the
prompt grows into a fuzzy finder over your command history; pick a command to
run it or to pull it into the edit buffer.

```
  git push origin
  git init
▌ git checkout -b new-branch
❯ gi
```

History is kept in its own SQLite database, separate from `$HISTFILE`, along
with the exit code, duration and working directory of every run. Ranking uses
those: recent, frequent, successful commands and commands you have run in the
current directory rank higher.

## Install

Requires a Rust toolchain.

```sh
cargo install --path .
```

Add this to `~/.zshrc`, after anything that sets your prompt (such as
`starship init`):

```zsh
eval "$(zray init zsh)"
```

Plugin managers can load `zray.plugin.zsh` instead; it does the same thing and
still needs the `zray` binary on `$PATH`.

Then import your existing history once:

```sh
zray import            # reads $HISTFILE, or ~/.zsh_history
zray import path/to/history
```

Importing the same file again adds nothing.

## Usage

Press <kbd>Up</kbd> at the prompt. Whatever is already typed becomes the
initial query. In a multi-line buffer, <kbd>Up</kbd> moves the cursor as usual
until it is on the first line.

| Key | Action |
| --- | --- |
| <kbd>Enter</kbd> | Run the selected command |
| <kbd>Tab</kbd> | Put the selected command in the edit buffer |
| <kbd>Up</kbd> / <kbd>Ctrl-P</kbd> | Move the selection up |
| <kbd>Down</kbd> / <kbd>Ctrl-N</kbd> | Move the selection down |
| <kbd>Esc</kbd> | Clear the query; if it is already empty, close |
| <kbd>Ctrl-C</kbd> / <kbd>Ctrl-G</kbd> | Close |
| <kbd>Ctrl-W</kbd> / <kbd>Ctrl-U</kbd> / <kbd>Ctrl-K</kbd> | Delete word / to start / to end |
| <kbd>Ctrl-A</kbd> / <kbd>Ctrl-E</kbd>, arrows | Move the cursor in the query |

Closing leaves the edit buffer as it was. With an empty query the list is
newest first, so <kbd>Up</kbd> <kbd>Enter</kbd> reruns the last command. When
nothing matches, <kbd>Enter</kbd> puts the query text in the edit buffer
rather than running it.

The query uses fzf-style syntax: space-separated terms must all match,
`'term` matches exactly, `^term` and `term$` anchor, `!term` excludes.
Matching is case-insensitive unless the query contains an uppercase letter.

## Configuration

Set these in `~/.zshrc` before the `eval` line.

| Variable | Default | Effect |
| --- | --- | --- |
| `ZRAY_HEIGHT` | `10` | Height of the search area in rows, including the input line. Must be exported. |
| `ZRAY_PROMPT` | last line of `$PROMPT` | Text shown before the query. |
| `ZRAY_NO_BIND` | unset | If set, <kbd>Up</kbd> is not bound. Bind the `zray-search` widget yourself. |
| `ZRAY_DB` | `$XDG_DATA_HOME/zray/history.db`, or `~/.local/share/zray/history.db` | Location of the database. Must be exported. |

To use a different key:

```zsh
ZRAY_NO_BIND=1
eval "$(zray init zsh)"
bindkey '^R' zray-search
```

## What gets recorded

Every command you run, from the moment the integration is loaded. Commands
starting with a space are skipped when `HIST_IGNORE_SPACE` is set. zsh's own
history is left untouched.

## Limitations

- Right-side prompts and transient prompts are not handled.
- A query wider than the terminal is not scrolled horizontally.
