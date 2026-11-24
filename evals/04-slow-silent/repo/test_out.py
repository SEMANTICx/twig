v = open("out.txt").read()
assert v == "hello", repr(v)
print("ok")
