from tax import split

def test_split_adds_up():
    assert round(sum(split(10.00, 3)), 2) == 10.00, split(10.00, 3)

def test_split_is_even():
    shares = split(10.00, 3)
    assert max(shares) - min(shares) <= 0.01, shares
