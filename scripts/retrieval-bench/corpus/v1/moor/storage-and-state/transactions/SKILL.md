---
name = "Transactions and conflict retry"
brief = "How a mooR transaction starts, what it reads, how commit detects conflict, and what a retry re-runs — plus what a retry does not undo."
when_to_use = "Use when a commit conflicts, a task retries or reports 'Transaction conflict', or you must reason about isolation, lost updates, or transaction boundaries in the mooR daemon. Not for the scheduler's queues, ticks and suspend rules (read moor/execution/task-scheduler), and not for the Torchship database."
universal = false
tags = ["moor", "transactions", "mvcc", "conflict", "retry", "commitresult", "conflictretry", "commit", "commit pipeline", "isolation", "visibility", "snapshot isolation", "optimistic concurrency", "rollback", "moor-db", "write skew", "lost update"]
version = 2
---
