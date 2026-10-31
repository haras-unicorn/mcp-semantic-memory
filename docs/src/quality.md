# Measuring quality

A note for a later issue. Nothing here is built yet. The defaults for the search
weights, the distance threshold and the consolidation parameters are still
guesses, and this is how they would stop being guesses.

## Data: synthetic only

The data is made up, never real memories. This is a public repository, and
memories are personal, so publishing real ones would be a privacy invasion even
when they look harmless.

An anonymized sample of the maintainer's own memories could be added later. That
needs its own look at what anonymized means, and it is not planned.

A generator makes the data from a fixed seed, so every run is the same and two
runs can be compared. The seed is only in the generator. The service itself
stays free of randomness.

## What is measured

The generator knows the truth, because it made it. It draws a few topics, writes
memories on them with some noise, repeats some, contradicts some, and spreads
them over time. It also writes queries with the memories that should come back.

- **Search.** For each query: is the right memory in the first few results, and
  how high is it? Measured with and without the reranker, and for each search
  weight, so the numbers show what each part is worth.
- **Interests.** Do the memories of one topic land in one cluster? Do two topics
  stay apart? The generator's topic labels are the answer key.
- **Upkeep.** Do repeats merge, do old unused memories fade and do used ones
  stay? Does a second consolidation change nothing? The last one is a plain test
  now that nothing is random.

The noise has a knob. Low noise checks that the code is right. High noise shows
where the defaults break.

Two kinds of vectors are worth trying. Drawn vectors (topic centres plus noise)
are cheap and exactly controlled. Real embeddings of generated text come from
the real model, so they show how the model behaves, but they are slower and
depend on the model.

## Tuning

Searching for good defaults automatically, for example with an evolutionary
search over the config, is only an idea. Whether it is the right approach is not
settled.

The catch is that synthetic data, with its own kind of noise, may not look like
real data. A search can fit that noise. So the result is a starting point for
the defaults, not proof that they are right, and it is the best available
without real data.

This note and the consolidation run time (how long a consolidation takes on a
large database) are the two items for the follow-up issue.
