# Embedding and rerank servers

The server holds no models. Embeddings come from an OpenAI-compatible server,
and reranking, which is optional, from a Jina-style one. The address in the
configuration is the server's own, without a path.

The embedding server has to speak the OpenAI embeddings API (`/v1/embeddings`),
which llama.cpp, vLLM and Ollama do. The rerank server has to speak the Jina
rerank API (`/v1/rerank`), which Jina, Cohere and llama.cpp do. OpenAI has no
rerank API.

## With llama.cpp

A `llama-server` runs one model, so the embedding model and the reranker each
get their own.

```sh
llama-server -m embedding.gguf --embedding --pooling mean --port 8080
llama-server -m reranker.gguf --reranking --port 8081
```

Then point the configuration at them:

```toml
[embedding]
url = "http://localhost:8080"
model = "nomic-embed-text-v1.5"
dimension = 768

[rerank]
url = "http://localhost:8081"
```

`embedding.dimension` has to be what the embedding model produces. The pooling
of the embedding model must not be `none`; a model's card says which pooling it
was trained for. The reranker needs `--reranking`, or
`--embedding --pooling rank`, which is the other spelling of it.

## What the server does with them

- Every text that is stored or searched is embedded by one request. The prefixes
  `embedding.query_prefix` and `embedding.document_prefix` go before queries and
  memories.
- When the reranker fails or answers with the wrong number of scores, search
  keeps the order it had and logs a warning. Reranking is an extra, so it never
  makes a search fail.
- When the embedding server fails, the call fails with the reason.
- When a vector has another length than `embedding.dimension`, the call fails
  and says so.

The model label is part of the embedding fingerprint. See
[Changing the embedding model](maintenance.md#changing-the-embedding-model) for
what to do when the model changes.
