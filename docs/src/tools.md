# Tools

The server has three tools, and a fourth that is offered only when
`consolidation.has_tool` is on. Each answers with structured JSON. A failed call
is a tool error with the reason as text, so the agent can read it and react.

When the startup checks find something to fix, the server still starts, but
every call fails with the reason, for example that `reembed` or `consolidate`
has to run. The checks are in [Maintenance](maintenance.md). The server checks
again on every call while there is a problem, so it serves again as soon as the
problem is fixed, with no restart.

## `memory_store`

Tells the memory something. The arguments:

- `event`: what happened or what was learned, as one self-contained piece of
  prose.
- `metadata`: optional, free form. It is kept with the event as it is.
- `decision`: optional. Only sent on the second call, see below.

If nothing stored is close to the event, it is stored. The answer has
`status: "written"` and the `memory_id`.

If memories close to it already exist, nothing is stored yet. The answer has
`status: "needs_decision"` and lists those memories as `candidates`, each with
its `memory_id`, `version` and text. The agent calls `memory_store` again with
the same `event` and `metadata` and a `decision`:

- `{"kind": "new"}`: the event is a memory of its own.
- `{"kind": "attach", "memory_id": 3, "version": 2, "memory_text": "..."}`: the
  event belongs to the candidate. `memory_text` is the full new text of the
  memory, its old text rewritten to include the event. `version` is the one the
  candidate had.
- `{"kind": "repeat", "memory_id": 3, "version": 2}`: the event only repeats the
  candidate, whose text stays.

The second call is the only way to change a memory. To correct or retire
something a memory says, the agent stores an event that says so and attaches it
with the corrected text. If a candidate changed between the two calls, the
answer is `needs_decision` again, with fresh candidates. Event and memory text
over `write.max_text_chars` is refused, with a message asking the agent to
condense it.

## `memory_search`

Finds memories by meaning and by words, best match first. Reading never changes
anything. The arguments are `query` and an optional `limit`, which defaults to
`search.limit`.

## `memory_recall`

Returns one memory by `memory_id`. With `with_events`, it also returns the
events told to it, newest first and capped at `search.max_events`, so the agent
can see how the memory formed over time. A memory that does not exist is a tool
error.

## `memory_consolidate`

Only there when `consolidation.has_tool` is on, which it is by default. It is
for a deployment with no timer to run `consolidate`, and it is not for routine
use. Its description tells the agent to call it only after another tool failed
because the last consolidation is too old. While that is the only problem, this
tool works and the others do not. Memories that need a `reembed` block it too,
since clustering mixed vectors means nothing.

It takes no arguments, runs the upkeep and applies it, and may take a while. The
answer is the same report that `consolidate` prints:

- `status`: `written` when the upkeep ran, `not_needed` when another run had
  just done it, `nothing_to_consolidate` when there are no memories.
- `memories`: how many active memories there were.
- `interests`: what the memory is interested in now, each with `top_terms`, the
  number of `memories` that belong to it and the text of the `nearest` memory.
- `forgotten`: the memories that faded, each with its `memory_id` and `text`.
  They come back when an event is attached to them again.

Two calls at once run the upkeep once. See
[Maintenance](maintenance.md#consolidating) for what the upkeep does.
