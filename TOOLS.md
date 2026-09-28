Saffrodex tools
===============

These are the model-callable tools added by Saffrodex layers.
Tools supplied by upstream Codex are outside this list.

`saffron.await_exec`
--------------------

Waits for a running `exec_command` session to produce output or exit.
Returned output is consumed; the tool does not send input or terminate the
process.

```cpp
{
  // Running session returned by exec_command.
  "session_id": int,
  // Event that ends the wait. Defaults to "output_or_exit".
  "return_on": ("output_or_exit" | "exit")?,
  // Independent wait deadline in milliseconds, from 1 through 4,294,967,295.
  // Reaching the deadline does not terminate the process.
  "timeout_ms": int?,
  // Non-negative output limit. Defaults to 10,000 and may be reduced by policy.
  "max_output_tokens": int?,
}
```

`saffron.edit_active_goal`
--------------------------

Replaces the objective of an active durable goal while preserving its
identity, status, budget, and accumulated usage.
Root threads and goal supervisors can use it when goal state is available.

```cpp
{
  // Complete replacement objective within the authorized scope.
  "objective": string,
}
```

`saffron.resume_goal`
---------------------

Resumes the root thread's paused, blocked, or usage-limited durable goal after
the user or system requested resumption.
The goal retains its identity, objective, budget, and accumulated usage.

```cpp
{}
```

`saffron.set_completion_delivery`
---------------------------------

Lets a spawned subagent choose whether its successful terminal result starts
a new turn for an idle parent or waits for the parent's next natural turn.
The default is `wake_parent`; failures and abnormal termination always wake
the parent.

```cpp
{
  // How the successful terminal result reaches the parent.
  "delivery": ("wake_parent" | "defer_to_parent"),
}
```

`saffron.supervisor_followup_parent`
------------------------------------

Lets the hidden goal supervisor wake its parent with a concrete next task when
the active goal can make useful progress.

```cpp
{
  // Next task and the evidence the parent needs to resume.
  "message": string,
}
```

`saffron.supervisor_snooze`
---------------------------

Lets the hidden goal supervisor schedule a later check when every unfinished
part of the goal depends on a future boundary or an already-running non-human
process and no useful parent action remains.

```cpp
{
  // Delay from 1 through 2,592,000 seconds.
  "delay_seconds": int,
  // Process or scheduled boundary that prevents useful parent action.
  "reason": string,
}
```

`saffron.supervisor_compact_parent_context`
-------------------------------------------

Lets the hidden goal supervisor request compaction for an idle parent after an
ignored corrective follow-up when context pressure prevents useful progress.

```cpp
{}
```

`saffron.supervisor_close_self`
-------------------------------

Lets the hidden goal supervisor mark the parent's active goal complete after
the available evidence establishes that no required work remains.

```cpp
{
  // Completion summary delivered to the parent.
  "message": string?,
}
```
