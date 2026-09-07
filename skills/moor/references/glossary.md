# mooR vocabulary

The words this codebase uses, and what each one means here. Many are inherited from
LambdaMOO and carry thirty years of assumed meaning. A word used loosely in this
system causes a real defect, because the same word often names a database concept, a
language concept, and a process at the same time.

Read this before your first change. It is the cheapest way past the vocabulary
barrier.

## The world

| Term | Meaning here |
|---|---|
| MOO | Two things. The kind of system: a multi-user, programmable, persistent shared world. And the language its verbs are written in. Context decides which. |
| LambdaMOO | The 1990s server this project re-implements. It is the compatibility target, not the design model. |
| ToastStunt | A later LambdaMOO fork. Some of its extensions are supported; support is partial by intent. |
| core | A database that holds a working world: its objects, its verbs, and its conventions. A server without a core starts but has nothing to be. Bundled cores live under `cores/`. |
| world state | The current contents of the database as the running system sees it: every object, property, and verb, under a transaction. |
| object | A persistent, permissioned entity with a number, an owner, a parent, properties, and verbs. |
| player | An object flagged as able to hold a connection. Not a separate type. |
| wizard | An object flag that grants full authority. Treat any code that runs with it as trusted. |
| programmer | An object flag that permits compiling verbs. |
| parent | The single object a given object inherits properties and verbs from. Inheritance is by prototype, not by class. |
| ancestry | The chain of parents from an object to the root. Property and verb lookup walks it, so it is cached. |

## The language and its values

| Term | Meaning here |
|---|---|
| verb | A named, permissioned program attached to an object. The unit of authored behaviour. |
| property | A named, permissioned value slot on an object. |
| builtin | A function the engine provides, callable from MOO code. Builtins are implemented in Rust and grouped by subject. |
| var | The runtime value type. Every MOO value is one. Values are immutable. |
| symbol | An interned name. Used where a string would otherwise be compared and hashed repeatedly. |
| flyweight | An immutable, value-typed, lightweight entity with slots, a contents list, and a delegate object that answers its verb calls. It has no object number and is not stored in the object table. Use it where an object would be too heavy. |
| error as value | A MOO error is an ordinary value that can be returned, stored, and tested, as well as raised. |
| opcode | One instruction of the compiled form of a verb. |
| program | The compiled form of a verb, as stored in the database. |
| objdef | The directory-based, version-controllable source format for a database. |
| textdump | The single-file LambdaMOO database format. |

## Execution

| Term | Meaning here |
|---|---|
| task | The unit of execution *and* the unit of transaction. Almost every question about correctness in this system reduces to where a task begins and ends. |
| tick | The unit of execution cost. A task has a budget in ticks and in seconds. |
| activation | One frame of the call stack inside the VM. |
| suspend | A task stops and gives up its place, to be resumed later. |
| fork | A task starts a second task, to run later or concurrently. |
| permissions | The identity a task runs as when it reads or writes the database. Not the same as the verb's owner and not the same as the caller. |
| capability grant | An explicit delegation of authority, narrower than the wizard flag. |
| command parser | The engine code that turns a line typed by a player into a verb call, with a direct object, a preposition, and an indirect object. |
| matching | Resolving a name a player typed to an object they can see. |

## Processes and protocol

| Term | Meaning here |
|---|---|
| daemon | The process that holds the database, the scheduler, and the VM. One per world. |
| host | A process that speaks one user-facing protocol and relays to the daemon. The telnet host and the web host are the two main ones. |
| worker | A process the daemon delegates an outside-world operation to, such as an HTTP request or a file read. It exists so the daemon never blocks and never holds an unsafe capability. |
| session | The daemon-side handle for one connected participant. It carries output to that participant and survives more than a single request. |
| connection | The transport-level link a host holds. A connection may exist before it is associated with a player object. |
| presentation | Structured content a verb sends to a client for display beyond plain text. A rich client renders it; a line-oriented client degrades it. |
| enrollment | The procedure by which a host or a worker proves it may talk to a daemon and receives its credentials. |
| event log | The daemon's durable record of what was sent to participants, used to replay history to a client that reconnects. |

## Development

| Term | Meaning here |
|---|---|
| Moot | The declarative test harness. A Moot file states MOO input and expected output, and runs against a real world. |
| checkpoint | A durable snapshot of the database taken while the server runs. |
| single-process | The build that puts daemon and hosts in one binary. Convenient for development. It does not change the architecture; it only removes the process boundaries. |
