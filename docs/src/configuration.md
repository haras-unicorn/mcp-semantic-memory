# Configuration

The server reads an optional file and then the environment. The environment
wins. Pass the file with the global `--config <path>` argument. It can be TOML,
YAML or JSON, picked by its extension, and it must exist. Without the argument,
the server reads `mcp-semantic-memory.toml` in the current directory if there is
one.

Every setting is a key in a section, like `search.limit`. The environment
variable for it starts with `MCP_SEMANTIC_MEMORY__`, goes into a section with a
double underscore and is upper case: `MCP_SEMANTIC_MEMORY__SEARCH__LIMIT`. Keys
without a section have only the prefix: `MCP_SEMANTIC_MEMORY__TIMEOUT`.

A duration is a number with a unit, like `500ms`, `30s`, `5m`, `12h` or `30d`.
The defaults of the numbers are placeholders that are not tuned on real data
yet. A bad value stops the program at start with an error that names the key.

```toml
timeout = "30s"

[database]
url = "sqlite:///var/lib/memory.db"

[embedding]
url = "http://localhost:8080"
model = "nomic-embed-text-v1.5"
dimension = 768

[rerank]
url = "http://localhost:8081"

[search]
limit = 10
half_life = "30d"

[consolidation]
has_tool = false
```

## Required

| Key                   | Environment variable                        | Meaning                                         |
| --------------------- | ------------------------------------------- | ----------------------------------------------- |
| `database.url`        | `MCP_SEMANTIC_MEMORY__DATABASE__URL`        | Database URL. Its scheme picks the dialect      |
| `embedding.url`       | `MCP_SEMANTIC_MEMORY__EMBEDDING__URL`       | The OpenAI-compatible embedding server, no path |
| `embedding.model`     | `MCP_SEMANTIC_MEMORY__EMBEDDING__MODEL`     | A label for the embedding model                 |
| `embedding.dimension` | `MCP_SEMANTIC_MEMORY__EMBEDDING__DIMENSION` | How long the embedding vectors are              |

The scheme can be `postgres:`, `postgresql:`, `mariadb:`, `mysql:` (which means
MariaDB) or `sqlite:`.

The model label and the dimension, with the distance metric, make up the
embedding fingerprint. The server refuses to serve when stored vectors carry a
different fingerprint, and
[`reembed`](maintenance.md#changing-the-embedding-model) fixes that. The label
comes from you, so a different model file behind the same label is not noticed.

## Everything else

The embedding and rerank servers are described in
[Embedding and rerank servers](servers.md). Some embedding models expect a
different short prefix before queries and before documents. Check the model card
and set the two prefixes if it says so. Leave them empty otherwise.

| Key                         | Default | Meaning                                                |
| --------------------------- | ------- | ------------------------------------------------------ |
| `timeout`                   | `30s`   | Timeout for the embedding and rerank servers           |
| `rerank.url`                | unset   | The Jina-style rerank server. Unset means no reranking |
| `embedding.query_prefix`    | empty   | Text put before a query when embedding                 |
| `embedding.document_prefix` | empty   | Text put before a memory when embedding                |
| `database.max_connections`  | `4`     | The most connections one instance opens                |
| `database.min_connections`  | `0`     | Connections kept open while idle, no more than the max |
| `database.acquire_timeout`  | `10s`   | How long a call waits for a connection before it fails |
| `database.busy_timeout`     | `10s`   | How long SQLite waits for a lock                       |
| `write.max_text_chars`      | `2000`  | The longest event or memory text                       |
| `write.max_candidates`      | `5`     | The most candidates shown on a store                   |
| `write.max_distance`        | `0.25`  | How near (cosine) a memory must be to be shown         |
| `search.limit`              | `10`    | Results of a search when the call has no limit         |
| `search.candidates`         | `40`    | How many memories each half of a search fetches        |
| `search.max_events`         | `50`    | The most events a recall returns                       |
| `search.rerank_candidates`  | `20`    | How many of the best candidates are reranked           |
| `search.vector_weight`      | `1.0`   | How much the vector ranking counts in the fusion       |
| `search.keyword_weight`     | `0.5`   | How much the keyword ranking counts in the fusion      |
| `search.frecency_weight`    | `0.1`   | How much recent reinforcement lifts a memory           |
| `search.interest_weight`    | `0.1`   | How much closeness to the interests lifts a memory     |
| `search.rank_constant`      | `60`    | The constant of rank fusion. Higher flattens the ranks |
| `search.half_life`          | `30d`   | How long until recency counts half as much             |
| `reembed.batch_size`        | `32`    | How many memories go to the embedding server at once   |

## Consolidation

These keys are for [the upkeep run](maintenance.md#consolidating).

| Key                                   | Default | Meaning                                                                    |
| ------------------------------------- | ------- | -------------------------------------------------------------------------- |
| `consolidation.has_tool`              | `true`  | Offer the `memory_consolidate` tool. Turn off where a timer runs it        |
| `consolidation.max_age`               | `14d`   | `run` refuses to serve when the newest event is this far past the last run |
| `consolidation.sample_size`           | `2000`  | How many of the most weighty memories are clustered                        |
| `consolidation.k_base`                | `e`     | The base of the logarithm that turns the memory count into `k`. Above one  |
| `consolidation.k_min`                 | `2`     | The fewest interests                                                       |
| `consolidation.k_max`                 | `16`    | The most interests                                                         |
| `consolidation.iterations`            | `50`    | The most rounds of k-means. It stops sooner when nothing moves             |
| `consolidation.top_terms`             | `5`     | How many words name an interest                                            |
| `consolidation.batch_size`            | `500`   | How many memories are read from the database at a time                     |
| `consolidation.grace`                 | `14d`   | How old a memory must be before it can be forgotten                        |
| `consolidation.forget_below_frecency` | `0.1`   | A memory with less frecency than this may be forgotten                     |
| `consolidation.forget_below_interest` | `0.3`   | A memory with less interest than this may be forgotten                     |

A memory is forgotten only when it is old enough and below both limits. Ages and
recency are measured against the data and not against the clock, see
[Maintenance](maintenance.md#time-is-relative).

## Logs

Logs are JSON, one object per line, on stderr. The fields of an event sit next
to a human-readable `message`, so `jq` and `grep` work on the message and a
metrics pipeline can read the fields. `RUST_LOG` sets the level, and it is
`info` when unset.
