# Security policy

## Reporting a vulnerability

Report privately through GitHub's private vulnerability reporting: open the
repository's **Security** tab and choose **Report a vulnerability**. Please do
not open a public issue for anything exploitable.

What helps most, in rough order:

- what an attacker gains — a false cache hit, code execution, data disclosure
- the smallest reproduction you have
- `arc --version` and `arc doctor` output
- the platform, and whether remote cache or remote execution is involved

`arc doctor` is designed to be safe to paste into a report: it prints
configuration shape, not credentials.

## Supported releases

The most recent 1.x release. Fixes are issued as patch releases.

## What Arc treats as a security bug

- A **false cache hit**: any result reused when the current state was not
  authorized to reuse it.
- Remote bytes becoming trusted execution state without digest verification.
- Output restoration escaping the project, through `..`, an absolute path, a
  symlinked parent or a symlinked destination.
- A secret appearing in a log, an execution record, the CAS, the metadata
  database, `--json` output, `arc history` or `arc inspect`.
- A traced command gaining privilege it would not have had without Arc.
- An environment described as hermetic when host state leaked into it.

## What Arc does not claim

Arc is not a sandbox. The threat model, including what a remote worker can
reach and what remote mode sends over the network, is documented in
[docs/security.md](docs/security.md). Read it before deploying the shared cache
or a worker.

The reference cache and worker servers support bearer-token authentication
(`--token-env`) but do **not** require it, and they terminate no TLS of their
own. Both bind to localhost by default, and both warn on startup if they are
serving an unauthenticated port on a reachable address. Running one open on an
untrusted network is a deployment mistake, not a vulnerability — but a way to
bypass a token that *is* configured, or to reach a path outside the worker's
workspace, is.
