#!/bin/sh
python3 test_duration.py >/dev/null || exit 1
git diff --quiet -- test_duration.py docs || { echo "test or spec edited"; exit 1; }
python3 - <<'PY'
from duration import parse_duration as p
ok = {"1.5h": 5400, "1H 30M": 5400, "1d2h3m4s": 93784, "0s": 0}
for text, want in ok.items():
    got = p(text)
    assert got == want and type(got) is int, (text, got)
got = p("0.5s")
assert got == 0.5 and type(got) is float, ("0.5s", got)
for bad in ["30m1h", "1h1h", "-5s", "5x", "h", "   "]:
    try:
        p(bad)
    except ValueError:
        continue
    raise AssertionError(f"{bad!r} must raise ValueError")
print("ok")
PY
exit 0
