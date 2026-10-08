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

`saffron.archive_self`
----------------------

Schedules archival of the calling conversation after its current turn finishes
successfully and the final response is saved.
It takes no target ID and requires a saved root in a running app-server.
Desktop does not need to be open.

```cpp
{}
```

The response is `{"status":"scheduled"}`, not confirmation of archival.
Finish the final response normally after calling the tool.
Interruption, failed completion, accepted steering, a newer turn, or server
restart cancels the pending request.
Native archival also archives spawned descendants; independent forks remain
unchanged.
The usual archive notification confirms completion.
An archive failure reports a warning and leaves the saved conversation available;
it may need to be reopened if shutdown already completed.
Use only when the user has requested archival.

`saffron.fork_thread`
---------------------

Starts a persistent independent root from the calling root's completed history.
The current unfinished turn is excluded; `prompt` supplies the new assignment.
The assignment arrives as attributed `saffron.fork_thread` output, not as a
human user message. Sender identity comes from the calling thread.
The fork inherits model settings, working directory, and permissions,
but does not inherit or automatically create a goal.
`reasoning_effort` can override the inherited effort without changing the model.
Execution belongs to the host and does not wait for Desktop placement.
The tool returns submission and placement outcomes without waiting for the fork
to finish, and does not send a subagent completion notification.

```cpp
{
  // Assignment to start after the inherited history.
  "prompt": string,
  // Optional persistent name for the new thread.
  "title": string?,
  // Effort supported by the inherited model, such as "low", "high", or "xhigh".
  // Omit to inherit the caller's effective effort.
  "reasoning_effort": string?,
  // Inherit the caller's Desktop sidebar section when available. Defaults true.
  "inherit_section": bool?,
  // Named destination; overrides inherit_section and creates it if absent.
  "section": string?,
}
```

The response includes `thread_id`, `status`, and a separate `section` outcome.
`status: "started"` includes the accepted `turn_id`.
`status: "created_not_started"` includes an error and the saved thread's ID,
which callers should use for recovery instead of creating another fork.
Section status is `inherited`, `placed`, `skipped`, or `failed`;
the latter two include a reason.
`placed` reports the destination `section_id` and whether it was `created`.
An explicit `section` overrides inheritance, including `inherit_section: false`.
Names are matched case-sensitively after trimming surrounding whitespace;
blank names are rejected before creating a fork.
A unique matching section is reused; an absent name creates a new section.
Duplicate names report failure without selecting or creating a destination.
Placement requests Desktop list and move handlers,
plus the creation handler only when a named destination is absent.
It preserves advertised tool spellings when available and otherwise uses
namespaced handler names without changing the caller's tool catalog.
The client may reject a request; a timeout reports an unconfirmed outcome.
Each request waits at most 60 seconds, and either condition reports failure.
Creation and moving are separate effects: a section may remain after a failed
move, and requests are not retried automatically.
Explicit placement never falls back to the caller's section.
Placement failure never cancels the fork or its assignment.

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
