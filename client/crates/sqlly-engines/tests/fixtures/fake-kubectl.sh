#!/bin/sh
set -eu

joined="$*"

case "$joined" in
  *"config get-contexts"*)
    printf '%s\n' "dev-cluster" "prod-cluster"
    exit 0
    ;;
  *"get ns"*)
    printf '%s\n' "namespace/default" "namespace/data"
    exit 0
    ;;
  *"get pods"*)
    printf '%s\n' "pod/postgres-0" "pod/mysql-0"
    exit 0
    ;;
  *"get svc"*)
    printf '%s\n' "service/postgres"
    exit 0
    ;;
  *"get deploy"*)
    printf '%s\n' "deployment/postgres"
    exit 0
    ;;
  *"port-forward"*)
    target=""
    remote="5432"
    for arg in "$@"; do
      case "$arg" in
        pod/*|service/*|deployment/*) target="$arg" ;;
        :*) remote="${arg#:}" ;;
      esac
    done
    name="${target#*/}"
    if [ "$name" = "missing" ]; then
      echo 'Error from server (NotFound): pods "missing" not found' >&2
      exit 1
    fi
    if [ "${FAKE_KUBECTL_HANG:-}" = "1" ]; then
      sleep 300
      exit 0
    fi
    port="${FAKE_KUBECTL_PORT:-45999}"
    if command -v python3 >/dev/null 2>&1; then
      exec python3 -c "import os,sys,time; print(f'Forwarding from 127.0.0.1:{os.environ.get(\"FAKE_KUBECTL_PORT\",\"45999\")} -> ${remote}', flush=True); \
[time.sleep(1) for _ in iter(int, 1)]"
    fi
    echo "Forwarding from 127.0.0.1:${port} -> ${remote}"
    trap 'exit 0' TERM INT
    while true; do sleep 1; done
    ;;
esac

echo "fake-kubectl: unhandled: $*" >&2
exit 1
