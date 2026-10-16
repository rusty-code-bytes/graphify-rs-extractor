# Graphify Rust Extractor

A disk-backed structural graph extractor for **PHP, SQL and HTML**. It produces a
`graph.json` that official Graphify can load and query, while keeping extraction
memory use low on large projects. No LLM or execution of project code is required.

This is an independent, early-stage implementation, not a complete port of
Graphify or a drop-in replacement for its incremental update protocol.

## Build and run

Requires stable Rust with edition 2024 support and a C compiler for SQLite and
tree-sitter. Python and Node.js are not required to build or run the extractor.

```sh
cargo build --release --locked

./target/release/graphify-extractor extract /path/to/project \
  --out /path/to/output \
  --workers 8 \
  --export-profile graphify \
  --parse-input-mib 8

# Small bundled example
./target/release/graphify-extractor extract examples/project --out graphify-out/demo

# All options
./target/release/graphify-extractor extract --help
```

The default output directory is `<project>/graphify-out`. A successful run
**replaces** its `graph.json`; it does not merge existing semantic annotations.
Reading, parsing or resolution failures before publication leave the previous
graph intact and return a nonzero exit code. Counts and coverage diagnostics are
printed to stderr and stored in `graph.json.graph.stats`.

## How it works

Files are discovered with exclusions applied before source extraction. Composer
loader metadata and referenced initialization files may still be inspected to
preserve loader precedence. Parsers write facts
to temporary SQLite databases; a global resolver connects declarations and
references, then streams the final JSON to an atomically published file. There
is no project-wide in-memory graph during extraction.

Parallel workers reuse threads and disk-backed shards. Results merge in input
order to preserve deterministic resolution and provenance. Resolution and export
remain serial, so eight workers do not imply eight fully occupied CPU cores.

| Language | Extracted structure |
|---|---|
| PHP | Declarations, calls, inheritance, traits, imports, includes, type references and statically resolvable receivers (including typed properties); namespace and Composer autoload context, duplicate symbols and `parent::` calls |
| SQL | Tables, columns, inline/ALTER foreign keys, views and their query dependencies, indexes and migration events; dialect- and context-aware table resolution |
| HTML | Local links, form actions and asset references to files present in the extracted corpus, including literal HTML within PHP templates |

Ambiguous or dynamic references are left unresolved rather than assigned an
arbitrary target. Installed Composer dependencies can change both graph coverage
and resolution. To include ignored `vendor/` directories, use `--no-gitignore`;
the extractor does not install dependencies or run Composer scripts.
Excluding a selected classmap/PSR dependency does not activate a visible duplicate.
Nested library manifests alone do not establish an active Composer loader;
runtime loader registration remains a static-analysis limitation. Unknown or
excluded autoload function sources, including delegated includes, can leave
external function calls unresolved rather than redirect them to a duplicate.
An earlier, proven `require` in the same execution scope can resolve an exact
function even with an unrelated unknown loader. This does not bypass namespace
fallback ambiguity. Computed include paths are limited to anchored expressions
and fresh local values before opaque effects; mutable global bootstrap paths
such as `$CFG->libdir` remain unproven.

## Filtering

```sh
./target/release/graphify-extractor extract /path/to/project \
  --exclude-file database/production.sql \
  --exclude 'backups/**' \
  --exclude-from excludes.txt
```

- `--exclude-file` is an exact project-relative path; `--exclude` is a glob. Both repeat.
- `--exclude-from` reads one project-relative glob per line. Blank lines and `#` comments are ignored; `!` negation is unsupported.
- A project-root `.graphifyignore` is loaded automatically. `*` stays within one directory; `**` crosses directories.
- `.gitignore` is respected by default, even outside Git repositories. `--no-gitignore` disables it; explicit exclusions still apply.
- Symlinks are not followed. `.git`, the output directory and the selected cache directory are excluded automatically.

## Memory and caching

| Option | Default | Purpose |
|---|---:|---|
| `--workers` | `1` | Concurrent parsing workers; increase for throughput at a memory cost |
| `--parse-input-mib` | `64` | Input-byte admission budget for parallel batches; oversized inputs run alone |
| `--max-file-mib` | `16` | Maximum PHP/HTML file or buffered SQL statement size, not whole SQL dump size |
| `--cache-dir` | disabled | Persistent cache of raw per-file facts |

These limits are **not a total RAM cap**: parser trees, facts and SQLite consume
additional memory. Temporary databases also require free disk space.

```sh
./target/release/graphify-extractor extract /path/to/project \
  --workers 8 --parse-input-mib 8 --cache-dir /path/to/extractor-cache
```

Cache keys include file contents, relative paths, parsing settings and an
implementation/grammar fingerprint. Every run rediscovers files and rebuilds
Composer context and global resolution, including after dependency changes or
exclusions. Cache payloads are checked before import. `GRE_CACHE` telemetry on
stderr reports hits, misses, writes and errors.

The first cache fill costs extra time and memory; subsequent runs still hash
inputs, resolve references and export the graph. Old cache entries are not
automatically evicted. The cache can be deleted when no extraction is using it.

## SQL dumps and migrations

SQL data payloads such as INSERT/REPLACE and PostgreSQL text COPY are streamed
without retaining their rows. This is a structural projection, **not data
validation or the final database state after executing migrations**.

`--sql-dialect` accepts `auto`, `mysql`, `postgres`, `sqlite` or `generic`. Auto
uses directory hints and otherwise MySQL. For dialect-sensitive syntax, select
the dialect explicitly.

Table resolution keeps separate schema/dump contexts and leaves equally ranked
conflicts unresolved. Recognized dump snapshots are isolated automatically;
nonstandard layouts can specify shared or per-file scopes:

```sh
./target/release/graphify-extractor extract /path/to/project \
  --sql-scope 'app/schema/**=app' \
  --sql-scope 'app/migrations/**=app' \
  --sql-scope 'backups/**=@file'
```

Scope rules affect resolution, not file inclusion. SQL inside PHP strings and
framework migration DSLs are not semantically analyzed. View dependencies respect
CTE scopes. Unsupported SQL syntax, including `CREATE VIEW ... AS TABLE ...` and
binary COPY, fails explicitly.

MySQL stored programs are syntax-checked and retained as `sql_statement` nodes.
Their bodies are not projected into dependency edges: these nodes explicitly
carry `extraction_coverage: "statement_only"`, counted in `sql_statement_only`.
An extracted program therefore does not imply complete analysis of its effects.
Literal `NO_BACKSLASH_ESCAPES` changes and bounded mysqldump mode save/restore
are supported per file. Unknown mode expressions or invalidated saved values
fail explicitly; the extractor does not execute stored programs to infer modes.

## Graphify compatibility and limitations

Output is directed NetworkX node-link JSON with `nodes` and `links`, supporting
multiple relations between the same endpoints. Nodes and edges retain source
provenance; call/import edges carry contexts used by official query filters.
Compatibility has been exercised with Graphify 0.9.91.

Use `--export-profile graphify` for official query, affected-node and path tools.
This projects ownership/import/trait relations into Graphify's vocabulary and
uses bare method labels for override traversal. IDs and resolved targets stay
unchanged; `qualified_name` and `original_relation` retain the original values
where projected. The default `native` profile retains qualified method labels
and native relations. Parameter, return and field contexts remain distinct,
even when they reference the same declaration.

```sh
graphify query "SomeClass" --graph /path/to/output/graph.json
graphify affected "SomeClass" --graph /path/to/output/graph.json
```

Use Graphify to **load and query the completed graph**. Rust entity IDs are not
compatible with Python's merge/watch/cache protocol. Do not run official
extract/update against this output expecting identities to remain stable.
Clustering, LLM enrichment and Python manifests are not generated. Graphify's
reader still loads the finished graph into RAM.

Static analysis cannot fully resolve runtime-generated autoloaders, dynamic PHP
calls or guarded/polyfill declarations without proof of the active definition. Hack files are explicitly inventoried
without symbol extraction. HTML parsing is tolerant and does not provide exact
line locations. This tool does not claim complete semantic coverage or identical
output to the Python extractor.

## Development and validation

```sh
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

Regression tests cover parser behavior, ambiguity, autoload selection, SQL scopes,
cache invalidation, parallel determinism and export profiles. Repaired scenarios
retain correct-target, forbidden-target and source-provenance assertions.

The `mysql_*_regressions` suites use the official MySQL 8.4 manual as a syntax
reference. They cover function expressions, statements and lexical variants,
with explicit cases for valid but unsupported syntax and atomic failure.
These are extractor tests, not MySQL server execution or function-result tests.
Streamed data statements are tested as counted omissions, not syntax validation.
Run the focused suites with:

```sh
cargo test --locked --test mysql_function_regressions \
  --test mysql_statement_regressions --test mysql_lexical_regressions \
  --test sql_coverage_regressions
```

Optional official-consumer integration tests require Graphify's Python dependencies:

```sh
python3 tests/graphify_consumers.py --upstream /path/to/graphify-source \
  --binary target/release/graphify-extractor --out /tmp/consumer-gate.json
```

Large-project checks are curated regression evidence, not a measurement of
whole-project semantic accuracy. Graphs intentionally change when missing
references are added or incorrect targets are removed.
Raw benchmarks, downloaded corpora and generated graphs are intentionally not
published in this repository.

Optional helpers (Python required):

```sh
python3 scripts/measure.py ./target/release/graphify-extractor extract examples/project
python3 scripts/check_graphify.py --upstream /path/to/graphify-source \
  --graph graphify-out/demo/graph.json --query Client
```

The second helper requires the upstream Graphify Python dependencies and uses
its loaders/query implementation directly.

The Rust API exposes `Options::new(root)` and `extract(&options)` from
`graphify_extractor`.

## License and bundled grammar

MIT; see [LICENSE](LICENSE). The patched `tree-sitter-php` 0.24.2 grammar is
intentionally included because its parser fixes are required for correctness.
Its original [license](vendor/tree-sitter-php/LICENSE),
[patch notes](vendor/tree-sitter-php/PATCHES.md) and grammar patch are retained.
Generated parser sources allow builds without a grammar generator. Other Rust
dependencies are fetched by Cargo and pinned in `Cargo.lock`.
