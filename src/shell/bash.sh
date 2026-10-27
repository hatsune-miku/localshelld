# Keep LF line endings: this Bash 3.2-compatible bootstrap is embedded verbatim.
# fd 3 is protocol input; fd 4 and fd 5 are private stdout/stderr frame channels.
exec 3<&0 4>&1 5>&2
# `builtin` bypasses any read/printf/eval a command may later define as a function.
while IFS='|' builtin read -r __localshelld_marker __localshelld_encoded __localshelld_input_encoded <&3; do
    builtin printf -v __localshelld_script '%b' "$__localshelld_encoded"
    builtin printf -v __localshelld_input '%b' "$__localshelld_input_encoded"
    # Route stdout/stderr separately through fd 4/fd 5, then close fd 3/4/5
    # for its duration: `exec >...` inside a command only rebinds its own fd 1, so
    # the next command still starts attached to the pipe, and the command cannot
    # forge or break the completion frame.
    builtin eval -- "$__localshelld_script" <"$__localshelld_input" >&4 2>&5 3<&- 4>&- 5>&-
    __localshelld_ec=$?
    __localshelld_cwd=$(builtin printf '%s' "$PWD" | /usr/bin/base64 | /usr/bin/tr -d '\r\n')
    builtin printf '\036%s|%s|%s\037' "$__localshelld_marker" "$__localshelld_ec" "$__localshelld_cwd" >&4
    builtin printf '\036%s|%s|%s\037' "$__localshelld_marker" "$__localshelld_ec" "$__localshelld_cwd" >&5
done
