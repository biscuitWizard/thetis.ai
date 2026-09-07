---
name = "The mooR command parser"
brief = "How a line a player types becomes a verb call: $do_command, word splitting, prepositions, object matching against the player's surroundings, and :huh."
when_to_use = "Use when working on command handling in the mooR server: $do_command, object matching against the player's surroundings, ambiguous or failed matches, ordinal matching such as \"second lamp\", or the :huh fallback. Not for the scheduler, the VM, permission rules, or writing a builtin. Not for telnet/web line handling, the Torchship database, or Thetis's internals."
universal = false
tags = ["moor", "command parser", "do_command", "dobj", "iobj", "prepstr", "argstr", "preposition tables", "object matching", "aliases", "huh", "parse_command", "find_command_verb", "dispatch_command_verb", "ambiguous match", "complex_match", "verb argument specs"]
version = 2
---
