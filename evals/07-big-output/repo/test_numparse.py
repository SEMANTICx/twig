import random
from numparse import parse

random.seed(7)
cases = []
for _ in range(1500):
    whole, frac = random.randint(1000, 999999), random.randint(0, 9)
    cases.append((f"{whole:,}.{frac}", whole + frac / 10))
for j, (whole, frac) in enumerate([(1234, 5), (98765, 4), (2000000, 1)]):
    cases.insert(700 + j * 13, (f"{whole:,}".replace(",", ".") + f",{frac}", whole + frac / 10))

failed = 0
for text, want in cases:
    try:
        got = parse(text)
        ok = abs(got - want) < 1e-6
    except Exception as exc:  # a crash is a failed case, not the end of the suite
        got, ok = f"{type(exc).__name__}: {exc}", False
    failed += not ok
    print(f"{'PASS' if ok else 'FAIL'} parse({text!r}) = {got!r} want {want!r}")
print(f"{len(cases) - failed}/{len(cases)} passed")
raise SystemExit(1 if failed else 0)
