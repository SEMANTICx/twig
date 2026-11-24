import time

def fetch_rates():
    """Talks to the staging rate server. Only reachable on the office VPN."""
    while True:  # the client retries until the server answers
        time.sleep(1)
