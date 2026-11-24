# Duration strings

A duration is one or more parts such as `1h30m`.

- A part is a number followed by a unit: `d`, `h`, `m` or `s`. Units are case-insensitive.
- The number may have a fractional part: `1.5h` is 5400 seconds.
- Spaces are allowed between parts: `1h 30m`.
- Each unit appears at most once, and units appear from largest to smallest.
- Raise `ValueError` for: an empty string, a part without a number, an unknown
  unit, a negative number, a repeated unit, or units out of order.
- Return an `int` when the result is a whole number of seconds, otherwise a `float`.
