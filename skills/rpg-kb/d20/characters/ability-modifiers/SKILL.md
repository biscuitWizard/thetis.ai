---
name = "D20 ability modifiers"
brief = "Convert an ability score to a modifier with floor((score - 10) / 2), including rounding negative halves down."
when_to_use = "Use when deriving the modifier added to checks, saves, attacks, damage, initiative, or other D20 totals from an ability score."
tags = ["d20", "ability score", "ability modifier", "formula", "strength", "dexterity"]
children = "none"
version = 1
---

# D20 ability modifiers

An ability's modifier is the score minus 10, divided by 2, rounded down. Thus average scores produce a modifier near zero, higher scores grant bonuses, and lower scores impose penalties. Recalculate the modifier whenever the score changes; do not add the score itself to a D20 roll.
