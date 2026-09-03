---
name = "Getting a world into and out of mooR"
brief = "Choose the right part of mooR's import and export path: objdef source directories, LambdaMOO textdump import, and the cores bundled in the repository."
when_to_use = "Use when a mooR database must be created, exported, updated or moved, or when a world starts but nobody can log in. Use it to pick the child skill. Not for the MOO language or the compiler, not for the transaction engine, not for the Torchship game database (the torchship skills own that), not for in-world verb authoring for a specific game, and not for Thetis's own internals."
universal = false
tags = ["moor", "moo", "objdef", "textdump", "import", "export", "core", "cowbell", "lambdacore", "lambda-moor", "minimal-core", "moorc", "moor-emh", "checkpoint", "load_object", "constants.moo", "bootstrap", "database", "--import", "--import-format", "objdef .moo directories", "import_export_id", "reload_object", "dump_object", "lambdamoo textdump", "toaststunt textdump"]
children = "auto"
related = ["moor/working-in-the-repo/build-and-run", "moor/storage-and-state/world-state-model"]
version = 2
---
