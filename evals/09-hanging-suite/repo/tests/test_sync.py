import os
from staging import fetch_rates

def test_rates_have_eur():
    if os.environ.get("OFFLINE") == "1":
        return
    assert "EUR" in fetch_rates()
