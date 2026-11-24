from slug import slug
assert slug("  Hello,  World! ") == "hello-world", repr(slug("  Hello,  World! "))
assert slug("Café Crème") == "cafe-creme", repr(slug("Café Crème"))
print("ok")
