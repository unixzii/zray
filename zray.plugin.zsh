# Entry point for zsh plugin managers. Needs the `zray` binary on $PATH
# (`cargo install --path .`).
if (( $+commands[zray] )); then
  eval "$(zray init zsh)"
else
  print -u2 "zray: binary not found on \$PATH; run 'cargo install --path .' in ${0:A:h}"
fi
