from money import cents
def total(items):
    return sum(cents(i) for i in items)
