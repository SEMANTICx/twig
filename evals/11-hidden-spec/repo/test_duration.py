from duration import parse_duration

assert parse_duration("90s") == 90
assert parse_duration("1h30m") == 5400
assert parse_duration("2m") == 120
try:
    parse_duration("")
except ValueError:
    pass
else:
    raise AssertionError("empty string must raise")
print("ok")
