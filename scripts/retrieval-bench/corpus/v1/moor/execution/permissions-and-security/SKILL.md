---
name = "mooR task permissions and the trust model"
brief = "Who a MOO task runs as, what the wizard and programmer bits grant, how permissions change across a verb call, and where no check is applied at all."
when_to_use = "Use when reasoning about authority in the mooR server: TaskPermissions, wizard/programmer/owner bits, object/property/verb flags, set_task_perms, capability grants, or whether an operation is checked at all. Use it before adding or changing a permission check. Not for the scheduler, opcode loop, writing a builtin, or command parsing. Not for storage, Torchship, or Thetis's internals."
universal = false
tags = ["moor", "permissions", "security", "wizard", "programmer", "owner", "task_perms", "set_task_perms", "caller_perms", "r/w/f object flags", "r/w/c property flags", "r/w/x/d verb flags", "capability grants", "property_read", "verb_call", "builtin_call", "e_perm", "authority"]
version = 2
---
