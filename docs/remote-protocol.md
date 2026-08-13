# The Arc remote cache protocol

Version 1. This document describes what Arc 0.5 actually implements; anything
not written here is not part of the protocol.

The protocol carries two kinds of thing: **objects**, which are opaque byte
strings identified by their BLAKE3 digest, and **execution records**, which say
which objects reconstruct the result of one execution. Objects are immutable and
globally deduplicated. Records are scoped to a namespace.

## Transport

HTTP/1.1 over `http` or `https`. Every path begins with `/v1`. A client that
receives a response from a different major version must not interpret it.

A client sends `Authorization: Bearer <token>` when a token is configured.
Redirects are not followed: the credential goes to the configured host or
nowhere.

## Identity

A digest is exactly 64 lowercase hex characters, the BLAKE3-256 hash of the
**uncompressed** object bytes. Compression is a property of a transfer, never of
identity.

A namespace is 1–128 characters from `[A-Za-z0-9._-]`, and is neither `.` nor
`..`.

An execution key is a digest-shaped string produced by the client. Its meaning
is Arc's, not the server's: the server treats it as an opaque key and only
checks its syntax.

## Endpoints

### `GET /v1/info`

Unauthenticated. Returns the server's protocol version.

```json
{ "protocol": 1, "server": "arc-cache", "version": "0.5.0",
  "encodings": ["arc-deflate"] }
```

### `POST /v1/{ns}/objects/missing`

Which of these objects does the server not have? At most 4096 digests per
request; a larger batch is `413`.

```json
{ "digests": ["<64 hex>", "..."] }      →      { "missing": ["<64 hex>"] }
```

Malformed digests are reported as missing rather than rejecting the batch,
because a client that cannot name an object cannot upload it either.

### `HEAD /v1/{ns}/objects/{digest}`

`200` if present, `404` if not.

### `GET /v1/{ns}/objects/{digest}`

Returns the object bytes. If the request carries
`Arc-Accept-Object-Encoding: arc-deflate` the server may answer with
`Arc-Object-Encoding: arc-deflate` and a raw-deflate body. Absence of the
response header means the body is the object itself.

A client **must** hash what it receives and compare it to `{digest}` before
using it. Content-Length, ETag and status code are not evidence.

### `PUT /v1/{ns}/objects/{digest}`

Uploads an object. The body may be deflate-compressed, declared with
`Arc-Object-Encoding: arc-deflate`.

The server **must** hash the decompressed bytes and reject the upload with `400`
unless they hash to `{digest}`. Without this, anyone permitted to write could
poison any digest.

Idempotent: uploading an object that already exists succeeds and changes
nothing. `201` on success.

### `GET /v1/{ns}/executions/{key}`

Returns the execution record, or `404`. A `404` here is an ordinary cache miss,
not an error.

### `PUT /v1/{ns}/executions/{key}`

Publishes a record. The server rejects it with:

- `400` if the record fails validation, or if its `execution_key` field does not
  equal `{key}`;
- `409` if any object it references has not been uploaded;
- `409` if a *different* record is already published under `{key}`.

Republishing a byte-identical record returns `200`. This makes concurrent
publication of the same result safe and makes contradictory results impossible
to introduce silently.

### `GET /v1/{ns}/tasks/{family_key}`

Returns one task record, or `404`.

### `PUT /v1/{ns}/tasks/{family_key}`

Publishes dependency knowledge for a family. `400` if the record fails
validation or its `family_key` does not equal `{family_key}`.

Unlike an execution record, a task record is an observation rather than a claim
of exclusivity, so a later publisher with at least as many observations replaces
an earlier one and neither is a conflict. Nothing downstream trusts it without
re-deriving the family key locally.

### `POST /v1/{ns}/tasks/lookup`

```json
{ "families": ["<64 hex>", ...] }
```

Returns `{"tasks": [...]}`, omitting families the server does not have. One
round trip per batch: a CI job asks about every task it intends to run at once,
and the answer is only worth having if obtaining it costs less than the work it
avoids.

A server that does not implement task knowledge answers `404`, which clients
treat as "no knowledge": the tasks stay unknown and therefore run.

## The execution record

```json
{
  "protocol": 1,
  "key_semantics": 3,
  "execution_key": "<64 hex>",
  "os": "linux",
  "arch": "x86_64",
  "program": "cargo",
  "args": ["test"],
  "rel_cwd": "",
  "family_key": "<hex>",
  "exit_code": 0,
  "duration_ms": 41230,
  "outputs": [
    { "path": { "enc": "utf8", "v": "target/debug/app" },
      "digest": "<64 hex>", "size": 8123456, "exec": true }
  ],
  "stdout": { "digest": "<64 hex>", "size": 214 },
  "stderr": { "digest": "<64 hex>", "size": 0 },
  "arc_version": "0.5.0"
}
```

`key_semantics` is Arc's cache-semantics version. A client must refuse a record
whose value differs from its own: the execution key is only meaningful under the
semantics that produced it.

`os` and `arch` are already folded into the execution key, so they cannot
mismatch in practice; they are carried anyway so a mismatch produces a stated
reason instead of a silent non-match.

Paths are project-relative and encoded explicitly. `enc: "utf8"` carries the
path as text; `enc: "b64"` carries the raw bytes, base64, for platforms where a
path need not be valid UTF-8. Display strings are never identity.

Field order is fixed by this document. The canonical byte form of a record is
its JSON serialisation in that order, which is what the server compares when
deciding whether two publications agree.

## Publish ordering

A client must:

1. ask which objects are missing,
2. upload those objects,
3. publish the record.

The record is a promise that its objects can be fetched. Publishing first would
let another client obtain a record it cannot replay. If step 3 fails the objects
from step 2 are orphaned; that is acceptable and the server may collect them.

## Fetch ordering

1. `GET` the record,
2. validate it,
3. subtract objects already held locally,
4. fetch the rest, verifying each,
5. only then replay.

If any step fails, the whole entry is unusable and the client executes locally.
Partial restoration is never correct.

## Limits

| Limit | Value |
|---|---|
| Metadata payload | 16 MiB |
| Output entries per record | 250,000 |
| Digests per `objects/missing` | 4,096 |
| Families per `tasks/lookup` | 512 |
| Paths per task record | 100,000 |
| Object size | 16 GiB |
| Namespace length | 128 |
| Path length | 4,096 |

## Errors

Failures carry `{"error": "..."}`. Clients retry `5xx` and transport faults a
small bounded number of times with exponential backoff; `401`, `403`, `404`,
`409` and malformed responses are never retried.

## The task record

```json
{
  "protocol": 1,
  "graph_semantics": 1,
  "dependency_semantics": 2,
  "trace_semantics": 1,
  "os": "linux",
  "arch": "x86_64",
  "family_key": "<64 hex>",
  "program": "cargo",
  "args": ["test", "-p", "arc-core"],
  "rel_cwd": "",
  "completeness": "complete",
  "inputs_narrowed": true,
  "produces": [{ "enc": "utf8", "v": "generated/client.ts" }],
  "consumes": [{ "path": { "enc": "utf8", "v": "src/lib.rs" }, "kind": "file" }],
  "declared_inputs": ["src/**"],
  "observations": 7,
  "arc_version": "0.6.0"
}
```

`program`, `args` and `rel_cwd` are present so a recipient can confirm the
record describes the task it already intends to run. **A recipient never learns
a command from here.** It derives the family key from its own checked-out
configuration, requires the record to carry that exact key, and requires the
command fields to match its local declaration byte for byte. Any mismatch, any
semantics difference, any unreadable path, and the record is discarded and the
task is treated as unknown.

Consequence, and the property this design exists to guarantee: a hostile server
can cause a client to do **more** work than necessary. It cannot cause a client
to run different work, and it cannot cause a client to skip work. Task knowledge
informs selection only — it never narrows a cache key.

## What the protocol does not do

There is no remote execution, no worker registration, no object enumeration, no
namespace listing and no deletion. A client learns object digests only from
records it is allowed to read.
