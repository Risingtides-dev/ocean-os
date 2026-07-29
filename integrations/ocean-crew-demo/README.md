# Ocean Crew — tangible local demo

This is a deliberately small proof, not the production Crew engine. It coordinates real existing Ocean daemon agent loops through `POST /v1/prompt` and adds only the missing demo-level workflow behavior:

- dependency-aware task dispatch;
- parallel ready workers;
- atomic durable JSON state;
- bounded retry;
- restart/resume of interrupted work;
- dependency-output handoff;
- visible terminal progress.

It calls no model provider directly. Every worker is an ordinary Ocean turn with its own persisted Ocean session.

## Run the three-turn demo

First confirm the supervised daemon is ready:

```bash
curl -fsS http://127.0.0.1:4780/ready | jq -e '.ok == true'
```

Then run two workers in parallel followed by their Crew lead:

```bash
python3 integrations/ocean-crew-demo/crew.py demo \
  --cwd "$PWD" \
  --state "$HOME/tmp/ocean-crew-demo-state.json"
```

Expected progress shape:

```text
[13:30:00] workflow   ocean-crew-tangible-demo id=...
[13:30:00] running    workflow-designer attempt=1/2
[13:30:00] running    failure-reviewer attempt=1/2
[13:30:02] completed  workflow-designer
[13:30:02] completed  failure-reviewer
[13:30:02] running    crew-lead attempt=1/2
[13:30:04] completed  crew-lead
[13:30:04] succeeded  ocean-crew-tangible-demo
```

Re-run the same command to resume or inspect the durable result:

```bash
python3 integrations/ocean-crew-demo/crew.py status \
  --state "$HOME/tmp/ocean-crew-demo-state.json"
```

To run again from scratch, choose a new state path. The adapter refuses to reuse a state file with a different workflow definition.

## Custom workflow

```bash
python3 integrations/ocean-crew-demo/crew.py validate \
  --workflow integrations/ocean-crew-demo/demo-workflow.json

python3 integrations/ocean-crew-demo/crew.py run \
  --workflow path/to/workflow.json \
  --state "$HOME/tmp/my-crew-state.json" \
  --max-workers 2
```

Workflow shape:

```json
{
  "name": "my-crew",
  "tasks": [
    {
      "id": "worker-a",
      "role": "researcher",
      "objective": "Return three findings.",
      "depends_on": [],
      "max_attempts": 2
    },
    {
      "id": "lead",
      "role": "crew lead",
      "objective": "Synthesize the completed reports.",
      "depends_on": ["worker-a"],
      "max_attempts": 2
    }
  ]
}
```

## Honest limitations

- This is an external demo adapter, not the production extension-owned Crew engine.
- Dispatch is **at-least-once** after an adapter crash. A turn may finish in the daemon after the HTTP client disappears; resume retries tasks that never durably recorded completion.
- It has no permission bridge. Demo prompts explicitly prohibit tools and use one turn. Tool-using production workflows still require the accepted permission and extension transport contracts.
- State is one local JSON file and assumes one demo process owns it.

Those limitations are kept visible so this proof demonstrates the actual desired interaction without pretending the remaining production work is already complete.

## Verification

```bash
python3 -m unittest integrations/ocean-crew-demo/test_crew.py
python3 integrations/ocean-crew-demo/crew.py validate \
  --workflow integrations/ocean-crew-demo/demo-workflow.json
```
