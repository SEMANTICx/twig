# Contributing

Run the suite with `OFFLINE=1 python3 run_tests.py`. Without `OFFLINE=1`,
`tests/test_sync.py` talks to the staging rate server, which is only reachable
on the office VPN; off the VPN it waits forever.
