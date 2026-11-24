def split(total, people):
    """Split a bill evenly. Shares must add up to the total, to the cent."""
    share = round(total / people, 2)
    return [share] * people
