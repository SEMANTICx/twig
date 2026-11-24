---
name: feature
when: add, build, implement, feature, new, support, wire
---

# feature

Build from the data shape outward. Name the shape before writing the code that
moves it.

## steps

1. **State the goal and the acceptance check** — one sentence each. The acceptance
   check must be a command whose output you can read.
2. **Name the data shape** at the boundary: what goes in, what comes out, what can
   be absent. Write it as a type or a literal example, not prose.
3. **Settle the caller's usage first.** Write the call site as you wish it read.
   Then make it compile. (This is the whole reason `/architect` exists.)
4. **Implement the smallest vertical slice** — one path end to end, not all paths half done.
5. **Prove it with the acceptance check.** Real output, not a claim.
6. **Prove it did not break the neighbours**: run the narrowest covering test.
7. **Write the journal cases** for the new behaviour now, while the intent is fresh.
8. **Commit** in ordered narrow commits, not one large one.

## skip rules

    skip: 3 — the call site already exists unchanged at src/render.tsx:88

## forbidden

- Adding a config flag for a value that never changes.
- Building the abstraction before the second caller exists.
- Declaring done on a green unit test that never exercised the real path.

## done when

The acceptance command passes, the journal has cases for the new path, and the
diff is smaller than you expected.
