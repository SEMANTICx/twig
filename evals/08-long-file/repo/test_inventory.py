from inventory import reorder_report

items = [
    {"sku": "A", "on_hand": 0, "threshold": 5},
    {"sku": "B", "on_hand": 5, "threshold": 5},
    {"sku": "C", "on_hand": 9, "threshold": 5},
]
got = reorder_report(items)
assert got == ["A", "B"], got
print("ok")
