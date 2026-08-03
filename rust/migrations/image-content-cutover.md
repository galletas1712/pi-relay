# One-time image artifact cutover

The deployed runtime accepts only durable image artifact references:

```json
{"type":"image","artifact_id":"sha256:<64 lowercase hex>"}
```

It has no reader for historical inline or URL image blocks. This operator-run
Rust cutover must therefore be applied before the daemon, runtime, and web
assets from this revision are started.

## Safety procedure

1. Take a `pg_dump` backup and verify that it can be read.
2. Stop the daemon, runtime, and every other database writer.
3. Do **not** remove or recreate containers, PostgreSQL volumes/data
   directories, or workspace directories.
4. Build the exact revision being deployed:

   ```sh
   cd rust
   cargo build --release -p agent-store --bin image-content-cutover
   ```

5. Supply the expected database name independently of its URL and run the
   shared validator:

   ```sh
   EXPECTED_DATABASE='pi_relay'
   ./target/release/image-content-cutover report \
     --expected-database "$EXPECTED_DATABASE" "$DATABASE_URL"
   ./target/release/image-content-cutover check \
     --expected-database "$EXPECTED_DATABASE" "$DATABASE_URL"
   ```

   Every mode checks `current_database()` before creating a table, locking a
   table, or scanning a row. `report` and `check` are equivalent read-only
   spellings. Both execute the same traversal and artifact admission as
   `apply`, then roll the transaction back.

   | Classification | Meaning |
   |---|---|
   | `convertible` | valid historical value that `apply` rewrites |
   | `canonical_valid` | applicable ref-only value whose artifacts exist and verify |
   | `opaque` | typed row/event path not owned by this migration |
   | `invalid` | ambiguous shape, bad image, missing/corrupt ref, or violated limit |

   Resolve every invalid row before applying.

6. Apply once:

   ```sh
   ./target/release/image-content-cutover apply \
     --expected-database "$EXPECTED_DATABASE" "$DATABASE_URL"
   ```

   One transaction creates and locks `public.image_artifacts`, scans the exact
   persistence paths below, inserts verified content-addressed rows, and
   rewrites JSON:

   | Table | Traversed path |
   |---|---|
   | `transcript_entries` | root user-message content and root tool result |
   | `actions` | typed root result for `kind='tool'`; exact reason/error/control bookkeeping stays opaque |
   | `queued_inputs` | tagged and historical untagged user messages |
   | `events` | user content in the input-event allow-list and typed `transcript.appended.entry.item` |

   Inline PNG/JPEG/GIF/WebP bytes are bounded, decoded, signature checked,
   hashed, deduplicated, inserted, then replaced with refs. Historical URL
   images become ordered text:
   `[remote image preserved as URL: <exact URL>]`. Historical tool `output`
   strings become one text content block. Historical `transcript.appended`
   events require matching top-level and `entry.item` copies; the nested copy is
   canonicalized and the redundant top-level copy is removed. Unknown envelope
   metadata, opaque provider replay, and exact interruption/control bookkeeping
   are untouched. Any late error rolls back artifact inserts and every prior
   JSON rewrite.

7. Run `check` again. It must report `convertible=0 invalid=0`. A second
   `apply` is an exact fixed point.
8. Deploy daemon, runtime, and web together. Load historical user/tool images,
   queued inputs, and a transcript export before resuming writers.

After the deployment is verified, delete this one-time binary, harness, and
runbook in a follow-up change.

## Disposable integration harness

The harness accepts only an administrator URL for a disposable PostgreSQL
server. It creates a random strict-prefix child database, installs the real
store schema, verifies a sentinel, tests wrong-database refusal and transaction
rollback, and drops only that child database in an exit trap.

```sh
PI_RELAY_TEST_ADMIN_DATABASE_URL='postgresql://postgres:postgres@127.0.0.1:55432/postgres' \
  sh rust/migrations/image-content-cutover-test.sh
```

Never point the harness at a deployment database.
