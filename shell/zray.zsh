# zray zsh integration, printed by `zray init zsh`.

zmodload zsh/datetime

typeset -g _zray_bin=__ZRAY_BIN__
typeset -g _zray_session="${EPOCHSECONDS}-$$"
typeset -g _zray_exec_id=

# --- recording ---------------------------------------------------------------

_zray_preexec() {
  _zray_exec_id=
  local cmd=$1
  [[ -n ${cmd//[[:space:]]/} ]] || return 0
  # Leading-space commands are private when HIST_IGNORE_SPACE is on.
  [[ -o histignorespace && $cmd == ' '* ]] && return 0
  _zray_exec_id=$("$_zray_bin" record start \
    --cwd "$PWD" --session "$_zray_session" --host "$HOST" -- "$cmd" 2>/dev/null)
}

_zray_precmd() {
  local exit_code=$?
  [[ -n $_zray_exec_id ]] || return 0
  "$_zray_bin" record end --id "$_zray_exec_id" --exit "$exit_code" \
    </dev/null >/dev/null 2>&1 &!
  _zray_exec_id=
}

_zray_chpwd() {
  "$_zray_bin" record dir -- "$PWD" </dev/null >/dev/null 2>&1 &!
}

autoload -Uz add-zsh-hook
add-zsh-hook preexec _zray_preexec
add-zsh-hook precmd _zray_precmd
add-zsh-hook chpwd _zray_chpwd
# The directory the shell starts in is entered without a chpwd.
_zray_chpwd

# --- search widget -----------------------------------------------------------

_zray_search() {
  # In a multi-line buffer, Up moves the cursor until it is on the first line.
  if [[ $LBUFFER == *$'\n'* ]]; then
    zle up-line
    return
  fi

  # Not named `prompt`: that is a special parameter tied to $PROMPT.
  local prompt_line
  if (( ${+ZRAY_PROMPT} )); then
    prompt_line=$ZRAY_PROMPT
  else
    prompt_line=$PROMPT
    [[ -o promptsubst ]] && prompt_line=${(e)prompt_line}
    prompt_line=${(%%)prompt_line}
    prompt_line=${prompt_line##*$'\n'}
  fi

  # The TUI owns the terminal through stdin/stdout; the result comes back on
  # fd 3, which is the command substitution's pipe. $TTY is the real device
  # rather than /dev/tty, which macOS cannot poll with kqueue.
  local out
  out=$("$_zray_bin" search --query "$BUFFER" --cursor "$CURSOR" --cwd "$PWD" --prompt "$prompt_line" \
    --result-fd 3 3>&1 <$TTY >$TTY)
  local action=${out%%$'\n'*} text=${out#*$'\n'}

  case $action in
    execute)
      BUFFER=$text
      zle reset-prompt
      zle accept-line
      ;;
    edit)
      BUFFER=$text
      CURSOR=${#BUFFER}
      zle reset-prompt
      ;;
    edit-path)
      BUFFER=${(q)text}
      CURSOR=${#BUFFER}
      zle reset-prompt
      ;;
    cd)
      # Change directory in place: nothing goes through the command line,
      # so the edit buffer survives and history gains no `cd` entry.
      if builtin cd -- "$text" 2>/dev/null; then
        # No command ran, so let prompts that are computed in precmd catch up.
        local hook
        for hook in precmd $precmd_functions; do
          (( $+functions[$hook] )) && $hook
        done
        zle reset-prompt
      else
        zle reset-prompt
        zle -M "zray: cannot cd to $text"
      fi
      ;;
    *)
      zle reset-prompt
      ;;
  esac
}

zle -N zray-search _zray_search

if [[ -z ${ZRAY_NO_BIND-} ]]; then
  () {
    local keymap seq
    for keymap in emacs viins; do
      for seq in '^[[A' '^[OA' "${terminfo[kcuu1]-}"; do
        [[ -n $seq ]] && bindkey -M $keymap "$seq" zray-search
      done
    done
  }
fi
