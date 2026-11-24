def parse(text):
    """Parse a human-written number such as '1,234.5' or '1.234,5'."""
    s = text.strip().replace(" ", "")
    s = s.replace(",", "")
    return float(s)
