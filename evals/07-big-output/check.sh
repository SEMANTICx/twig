#!/bin/sh
python3 test_numparse.py >/dev/null || exit 1
git diff --quiet -- test_numparse.py || { echo "test edited"; exit 1; }
python3 - <<'PY'
from numparse import parse
for text, want in {"1,234,567.89": 1234567.89, "1.234.567,89": 1234567.89, "42": 42.0, " 3.5 ": 3.5}.items():
    got = parse(text)
    assert abs(got - want) < 1e-6, (text, got, want)
print("ok")
PY
exit 0
