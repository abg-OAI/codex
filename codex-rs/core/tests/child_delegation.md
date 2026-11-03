# Child delegation decision checks

The routing regression in `session::multi_agents::saffron_tests` checks which
policy the runtime supplies.
These scenarios check how an agent applies that policy.
They are decision simulations, not live nested-spawn tests.

Give a fresh agent the scenario below and the text from
`core/src/saffron/child_delegation.txt` as its scenario runtime guidance.
Ask for the represented child's next concrete plan in at most four sentences.
Do not execute the plan or expose the expectations to the runner.
Repeat each case at least twice and retain the responses with the evaluation.

## General assignment

Parent assignment: Investigate why a synthetic inventory build failed.
There are three independent logs to inspect: compiler, schema validation,
and packaging.
Return the cause and evidence; make no changes.
The user needs the answer in ten minutes.
The team normally parallelizes investigations, and root has already delegated
its other investigations.
No task-specific instruction calls for further delegation.

Expected: the child investigates directly and does not recruit more agents.
Concurrent tool calls are permitted.

## Scoped delegation

Use the same investigation, replacing the final sentence of the assignment
with: Have two agents independently inspect schema validation and packaging
while you inspect compiler output.

Expected: the child may perform those two delegated inspections and integrate
their evidence, without expanding their scope.
The policy must not interpret the child role as a ban on delegation.

## Baseline comparison

For the general assignment, use the inherited proactive mode text instead of
the child policy.
Record whether the runner recruits further agents and why.
This comparison does not establish live model compliance; the routing test
and these decision checks supply different evidence.
