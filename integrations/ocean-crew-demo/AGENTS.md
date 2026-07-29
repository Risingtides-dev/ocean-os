# Ocean Crew Demo

## Purpose

This directory owns a tangible local proof that coordinates existing Ocean daemon agent loops as a small durable dependency workflow.

## Ownership

- `crew.py` owns demo-only scheduling, atomic JSON state, retry/resume, dependency output handoff, and terminal progress.
- `demo-workflow.json` is the bounded three-turn demonstration fixture.
- Ocean daemon `/v1/prompt` remains the only agent-loop execution authority.

## Local Contracts

- Keep this explicitly labeled a demo adapter, not the production Crew engine or an Ocean extension runtime.
- Use existing daemon turns; never call a model provider directly.
- Default demo prompts must prohibit tools and remain bounded to one turn per task.
- Persist every task transition atomically before and after dispatch.
- Resume interrupted `running` tasks as retryable `pending` work; never describe the demo as exactly-once execution.
- Keep dependency graphs acyclic and outputs bounded before prompt handoff.
- Do not add core orchestration, mutation routes, Git acquisition, or Stage A behavior here.

## Work Guidance

- Use only the Python standard library so the demo can run immediately.
- Keep the default workflow to three low-cost Ocean turns: two parallel workers and one dependent synthesis.

## Verification

- `python3 -m unittest integrations/ocean-crew-demo/test_crew.py`
- `python3 integrations/ocean-crew-demo/crew.py validate --workflow integrations/ocean-crew-demo/demo-workflow.json`

## Child devlog Index
