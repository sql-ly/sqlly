#!/bin/sh
# Stand-in for `aws ssm start-session` in tests/ssm_forward.rs. Records its
# arguments, starts a long-lived helper the way the real CLI starts
# session-manager-plugin, and prints the plugin's ready lines.
set -eu

: > "$FAKE_AWS_ARGS_FILE"
params=""
take_next=0
for arg in "$@"; do
  printf '%s\n' "$arg" >> "$FAKE_AWS_ARGS_FILE"
  if [ "$take_next" = 1 ]; then
    params="$arg"
    take_next=0
  fi
  if [ "$arg" = "--parameters" ]; then
    take_next=1
  fi
done

if [ -n "${FAKE_AWS_FAIL:-}" ]; then
  echo "An error occurred (TargetNotConnected) when calling the StartSession operation: i-0fake is not connected." >&2
  exit 254
fi

if [ -n "${FAKE_AWS_NO_PLUGIN:-}" ]; then
  echo ""
  echo "SessionManagerPlugin is not found. Please refer to SessionManager Documentation here: https://docs.aws.amazon.com/console/systems-manager/session-manager-plugin-not-found"
  exit 255
fi

port=$(printf '%s' "$params" | sed 's/.*"localPortNumber":\["\([0-9]*\)"\].*/\1/')

sleep 300 &
echo "$!" > "$FAKE_AWS_CHILD_FILE"

echo ""
echo "Starting session with SessionId: tester-0fake"
echo "Port $port opened for sessionId tester-0fake."
echo "Waiting for connections..."
wait
