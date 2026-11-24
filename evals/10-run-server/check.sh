#!/bin/sh
PORT=$(python3 -c "import socket; s=socket.socket(); s.bind(('127.0.0.1', 0)); print(s.getsockname()[1])")
PORT=$PORT python3 app.py >/dev/null 2>&1 &
PID=$!
trap 'kill $PID 2>/dev/null' EXIT
for _ in $(seq 50); do curl -s "http://127.0.0.1:$PORT/" >/dev/null && break; sleep 0.1; done
code=$(curl -s -o /tmp/twig-health-$$ -w '%{http_code}' "http://127.0.0.1:$PORT/health")
[ "$code" = 200 ] || { echo "status $code"; exit 1; }
python3 -c "import json,sys; assert json.load(open(sys.argv[1])) == {'status': 'ok'}" /tmp/twig-health-$$ || exit 1
[ "$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$PORT/")" = 200 ] || { echo "/ broken"; exit 1; }
echo ok
exit 0
