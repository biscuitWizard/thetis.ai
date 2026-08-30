---
name = "Objdef: a world as a directory of source files"
brief = "How mooR's objdef directories work: file layout, object identity, the multi-pass import, what round-trips, and how to load or replace one object safely."
when_to_use = "Use when you read, write, import, export or diff objdef .moo files, or when an import fails to compile a verb or reports a duplicate object. Not for the LambdaMOO textdump format (read textdump-compat), not for choosing or starting a core (read cores-and-bootstrap), and not for the MOO language, the Torchship game database, or Thetis's own internals."
universal = false
tags = ["moor", "objdef", "moo files", "constants.moo", "import_export_id", "load_object", "reload_object", "dump_object", "checkpoint", "export", "conflict", "clobber", "skip", "moor-objdef", "round trip", "version control", "import_export_hierarchy", "define declarations", "include!", "include_bin!", "property overrides", "verb and method blocks", "parse_objdef_constants", "moor-emh", "detect", "entity overrides", "duplicate object"]
related = ["moor/language-and-compiler/compiler-pipeline", "moor/storage-and-state/world-state-model"]
version = 2
---
