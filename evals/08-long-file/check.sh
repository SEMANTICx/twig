#!/bin/sh
python3 test_inventory.py >/dev/null || exit 1
git diff --quiet -- test_inventory.py || { echo "test edited"; exit 1; }
python3 -c "
from inventory import needs_restock, price_sku_123, price_sku_399
assert needs_restock(4, 5) and needs_restock(5, 5) and not needs_restock(6, 5)
assert price_sku_123(10) == round(10 * 2.23, 2) and price_sku_399(2) == round(2 * 4.99, 2)
print('ok')"
exit 0
