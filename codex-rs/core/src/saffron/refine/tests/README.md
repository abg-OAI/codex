# Prompt refinement behavioral tests

These fictional scenarios evaluate `../../refine_prompt.md`.
They complement the Rust tests in `../tests.rs`: Rust tests cover selection,
projection, and failure handling, while these cases evaluate the interpretation
produced by a model.

## Run and grade

1. Use a fresh isolated agent for each case and repetition.
   Supply the refinement prompt, the case's JSON input, and the output schema
   below. Withhold this README and the evaluator-only expectations.
   The runner may not execute the request, modify files, or access other context.
2. Prefer the Luna model used by the runtime when it is available.
   Record the model and revision, guidance revision, input, and raw response.
   A subagent evaluation does not by itself establish app-server transport,
   service tier, or attachment handling.
3. Repeat cases 01, 02, 03, and 07 three times and the other cases twice.
   Use new agents rather than continuing a conversation between repetitions.
4. Give a separate fresh judge the input, raw output, governing prompt,
   and evaluator-only expectations. Require evidence from input and output
   for each verdict. Judge semantic preservation, not a preferred wording.
5. Parse each response as JSON and require a single nonempty string field
   named `refinement`. Record every failure, the per-case pass counts,
   and any uncertainty. One failed repetition leaves that case unresolved.

The output schema is:

```json
{
  "type": "object",
  "properties": {"refinement": {"type": "string"}},
  "required": ["refinement"],
  "additionalProperties": false
}
```

Use [scenarios.md](scenarios.md) for inputs and held-out expectations.
If a case fails, classify whether the error belongs to the scenario,
evaluator, guidance, or runtime before changing the prompt.
For a guidance repair, retain the failing response, make the smallest change,
inspect the complete prompt for duplicated or conflicting requirements,
and rerun the failed case plus its adjacent cases against the integrated result.
Do not weaken a valid permission limit to make a test pass.
A baseline for new guidance omits the candidate while retaining the same input;
an update uses the preceding prompt revision as its baseline.
