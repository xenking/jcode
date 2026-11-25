# Runs in its own session. The inner shell may use exit/exec without bypassing
# the supervisor's receipt. The timer is part of the process group, not Jcode.
command=$1
receipt=$2
duration=$3
timeout_error=$4

write_receipt() {
    printf '%s' "$1" > "$receipt.tmp" && mv -f -- "$receipt.tmp" "$receipt"
}

if [ -n "$duration" ]; then
    trap 'write_receipt "timeout:$timeout_error"; printf "\n--- %s ---\n" "$timeout_error"; kill -KILL -- "-$$"' USR1
    (
        trap 'kill "$sleeper" 2>/dev/null; exit 0' TERM
        sleep "$duration" &
        sleeper=$!
        wait "$sleeper" && kill -USR1 "$$"
    ) &
    timer=$!
fi

bash -c "$command" &
job=$!
wait "$job"
status=$?
if [ -n "$duration" ]; then
    trap '' USR1
    kill "$timer" 2>/dev/null
    wait "$timer" 2>/dev/null
fi
write_receipt "$status"
exit "$status"
