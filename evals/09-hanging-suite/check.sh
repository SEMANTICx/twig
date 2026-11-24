#!/bin/sh
OFFLINE=1 timeout 60 python3 run_tests.py >/dev/null || exit 1
git diff --quiet -- tests run_tests.py || { echo "tests or runner edited"; exit 1; }
python3 -c "
from tax import split
for total, n in [(10.0, 3), (100.0, 7), (0.05, 4), (1.0, 1)]:
    s = split(total, n)
    assert len(s) == n and round(sum(s), 2) == total and max(s) - min(s) <= 0.0100001, (total, n, s)
print('ok')"
exit 0
