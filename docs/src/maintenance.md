# Maintenance

Two commands keep the memory in shape. `consolidate` is routine. `reembed` is
for the day the embedding model changes.

Neither is blocked by the startup checks, and neither needs the embedding server
except `reembed`. Stop the instances that serve the memory before `reembed`.
They refuse to serve anyway once the model in the configuration changed.

## Consolidating

A consolidation does three things:

1. It finds what the memory is interested in. It takes the most reinforced
   memories, up to `consolidation.sample_size`, and clusters their vectors with
   weighted k-means. Each cluster centre is an _interest_, named by its top
   terms: the words that are common in the cluster and rare in the rest of the
   memory, so filler words do not show up and no stopword list is needed.
2. It scores every memory by how close it is to the nearest interest.
3. It forgets the memories that are old enough (`consolidation.grace`) and low
   on both recency and interest. Nothing is deleted. A forgotten memory is
   skipped by search and recall, and it comes back when an event is attached to
   it again.

The interests of every run are kept, so how they moved over time can be read
from the `interests` table.

```sh
mcp-semantic-memory consolidate
```

This is a dry run. It writes nothing and prints the interests it would make,
each with its top terms, how many memories belong to it and the text of the
memory nearest to its centre, and the memories it would forget. Read it to see
whether the interests look right.

```sh
mcp-semantic-memory consolidate --apply
```

This writes. It refuses to run while memories have no vector from the current
embedding model, since clustering vectors from different models means nothing:
run `reembed` first.

### From a timer

Run it from a timer. The server refuses to serve once the newest event is more
than `consolidation.max_age` past the last run, which catches a timer that
stopped. On NixOS:

```nix
{ pkgs, mcp-semantic-memory, ... }:
let
  binary = "${mcp-semantic-memory}/bin/mcp-semantic-memory";
in
{
  systemd.services.mcp-semantic-memory-consolidate = {
    description = "Consolidate the semantic memory";
    serviceConfig = {
      Type = "oneshot";
      DynamicUser = true;
      EnvironmentFile = "/etc/mcp-semantic-memory.env";
      ExecStart = "${binary} consolidate --apply";
    };
  };

  systemd.timers.mcp-semantic-memory-consolidate = {
    wantedBy = [ "timers.target" ];
    timerConfig = {
      OnCalendar = "weekly";
      Persistent = true;
    };
  };
}
```

The environment file holds `MCP_SEMANTIC_MEMORY__DATABASE__URL` and the
embedding settings, the same as the serving instances. Set
`MCP_SEMANTIC_MEMORY__CONSOLIDATION__HAS_TOOL=false` on those, so the model is
not offered a tool that the timer already covers. Weekly is well inside the
default `max_age` of two weeks.

Any number of instances and timers may run at once. The write is one transaction
under a lock, so one of them writes and the others find the memory current and
stop.

### Without a timer

A deployment that is one binary on one SQLite file has no timer. With
`consolidation.has_tool` on, which is the default, the server offers a fourth
tool, [`memory_consolidate`](tools.md#memory_consolidate). When the last
consolidation is too old, every other tool fails and says to call it. It does
the same as `consolidate --apply` and answers with the same report. It is safe
to leave to the model: with the same data, a second run finds the same interests
and forgets nothing more.

A consolidation on a large memory can take longer than a client waits for a tool
call. How long it takes has not been measured on real data yet.

### Time is relative

Nothing in ranking, forgetting or the startup checks looks at the clock of the
instance. Recency is measured from the median time of the last event of the
memories being compared, the grace period and the age of the last run from the
newest event. So a quiet month ages nothing, one new event after a long gap does
not make everything old at once, and instances with different clocks agree. The
median, and not the average, so that a few very old or very new memories barely
move it.

## Changing the embedding model

Vectors from different models cannot be compared. The model label and the
dimension in the configuration make up the embedding fingerprint, and the server
refuses to serve when stored vectors carry another one. Every tool call then
fails with the number of memories to fix and the command to run.

1. Stop the instances that serve the memory.
2. Change `embedding.model`, `embedding.dimension` and the prefixes, and start
   the new embedding server.
3. Run `mcp-semantic-memory reembed`.
4. Run `mcp-semantic-memory consolidate --apply`, since the interests have no
   vectors until then.
5. Start the instances again.

`reembed` embeds every memory, forgotten ones too, with the new model, a page at
a time (`reembed.batch_size`), into a new table beside the live one. Nothing is
held in memory but the page. When it is done, and not before, it swaps the
tables and drops the interest vectors. The interest rows stay, so the history of
the interests is kept.

It can be stopped at any point. The live vectors stay as they were until the
swap, and running it again goes on with the memories that have no new vector. If
you change the model again before it finished, the half-made table is dropped
and it starts over. How the swap is done differs by database, see
[Databases](databases.md#reembed).

The label is yours to keep honest. If the model file behind an unchanged label
changes, nothing notices, and the old vectors stay in use.
