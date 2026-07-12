# Core runtime systems

These systems define how work moves and completes inside one engine. Read them
in contract order; the scheduler internals are rarely the first place to edit.

| Concern | Chapter | Owning layer |
|---|---|---|
| Addressing, kinds, delivery | [Mail and kinds](mail-and-kinds.md) | actor/data/substrate |
| What may run concurrently | [Concurrency and blocking](concurrency.md) | actor/substrate |
| Queueing and worker policy | [Scheduler](scheduler.md) | substrate internals |
| Ordered frame stages | [Frame lifecycle](lifecycle.md) | lifecycle capability/chassis |
| Causal completion | [Tracing and settlement](tracing-and-settlement.md) | substrate + trace capability |
| Per-actor evidence | [Logging](logging.md) | actor/substrate/MCP |
| Boot-time policy | [Configuration](configuration.md) | derives + chassis composition |

The stable public contract is mail plus actor seriality. Scheduler queues,
worker heuristics, and tuning defaults are implementation details unless an ADR
promotes them. When debugging, prove the failure layer before changing a knob:

```text
wrong name/schema → registry or kind issue
handler never runs → delivery/wiring issue
handler overlaps itself → actor invariant violation
descendants remain pending → settlement/hold issue
correct but slow → cost/scheduler/load issue
```

Changes here have wide fan-out. Read [invariants](../foundations/invariants.md),
the owning ADRs, and tests across both native and wasm actors before editing.
