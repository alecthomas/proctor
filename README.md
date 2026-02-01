# Proctor: A Process Manager with Hot Reload

## Overview

Proctor is a local development process manager that extends the Procfile format with file watching, hot reload, dependency ordering, and coloured log prefixing. It aims to replace ad-hoc combinations of `foreman`, `watchexec`, and shell scripts with a single declarative file.

## Procfile Syntax

### Line format

```
<proc> [!]<glob>... [option=value ...]: [ENV=VALUE ...] <command>
```

Each line defines a process. The colon (`:`) separates the **declaration** (left) from the **execution** (right). Tokenisation uses shell-style rules throughout: bare words, `'single quoted'` (literal), and `"double quoted"` (with escape sequences).

### Comments and blank lines

Lines starting with `#` are comments. Blank lines are ignored.

### Line continuation

A trailing `\` continues the command onto the next line, following shell conventions:

```
api **/*.go: go run \
  -tags dev \
  ./cmd/api
```

## Declaration (left of `:`)

### Process name

The first token is always the process name. It must be unique within the file. Valid characters: `[a-zA-Z0-9_-]`.

### Glob patterns

Any token containing glob characters (`*`, `?`, `{`, `[`) or a `/` is interpreted as a file watch pattern. Patterns follow standard globbing rules including `**` for recursive matching and `{a,b}` for alternation.

A token prefixed with `!` is an exclusion pattern.

```
api **/*.go !**_test.go !vendor/**:
```

If no glob patterns are present, the process is not file-watched.

### Options

Any token matching `key=value` that is not a glob and not the process name is an option. Values follow shell quoting rules.

| Option      | Type     | Default     | Description                                            |
|-------------|----------|-------------|--------------------------------------------------------|
| `after`     | string   |             | Dependency — wait for named process to become ready    |
| `ready`     | string   |             | Readiness probe (see [Readiness](#readiness))          |
| `signal`    | string   | `TERM`      | Signal to send on reload (`HUP`, `INT`, `TERM`, etc.) |
| `debounce`  | duration | `500ms`     | Debounce interval for file change events               |
| `dir`       | path     | `.`         | Working directory for the process                      |
| `shutdown`  | duration | `5s`        | Grace period after signal before SIGKILL               |

Multiple dependencies can be specified with comma separation: `after=redis,migrate`.

## Execution (right of `:`)

### Environment variables

Tokens matching `KEY=VALUE` before the command set environment variables for the process. These are merged on top of the inherited environment. Inline values take precedence.

### Command

Everything after environment variables is the command, executed via the system shell (`$SHELL` or `/bin/sh`). This means pipes, redirects, and subshells work as expected:

```
api: go run ./cmd/api 2>&1 | grep -v healthcheck
```

## Process Lifecycle

### Classification

Processes are classified based on context:

- **One-shot**: A process with no glob patterns. These are expected to run to completion.
- **Long-running**: A process with glob patterns, or a process that does not exit on its own. These are expected to stay alive.

### Startup order

1. Parse the Procfile and build a dependency graph from `after=` declarations.
2. Start all processes with no dependencies concurrently.
3. As each process becomes ready (exits 0 for one-shot, passes readiness probe or starts for long-running), start any processes that were waiting on it.

If a one-shot process exits non-zero, startup is aborted and all running processes are shut down. Circular dependencies are detected at parse time and treated as an error.

### Hot Reload

When a file change matches a process's glob patterns (after exclusions):

1. Debounce: wait for the configured debounce interval with no further matching changes.
2. Send the configured signal (default `SIGTERM`) to the process group.
3. Wait up to `shutdown` duration for the process to exit.
4. If still running after the grace period, send `SIGKILL`.
5. Restart the process.

### Restart on crash

If a long-running process exits unexpectedly (not due to a reload or shutdown):

- Restart immediately on first failure.
- Apply exponential backoff on consecutive failures: 1s, 2s, 4s, 8s, capped at 30s.
- Reset the backoff counter after 60s of successful running.
- Log each restart with the exit code or signal.

### Shutdown

On `SIGINT` or `SIGTERM` to proctor itself:

1. Send `SIGTERM` to all process groups in reverse dependency order.
2. Wait up to each process's `shutdown` duration.
3. `SIGKILL` any remaining processes.
4. Exit.

## Readiness

The `ready` option defines how proctor determines a process is ready (for `after=` dependents).

| Format                              | Behaviour                                          |
|-------------------------------------|---------------------------------------------------|
| `tcp://<port>`                      | Poll `localhost:<port>` until a connection succeeds |
| `http://:<port>[/<path>]`           | Poll `http://localhost:<port>[/<path>]` for a 2xx response |
| (none)                              | Long-running: ready immediately on start. One-shot: ready on exit 0. |

Readiness polling begins when the process starts, with a 250ms interval and a 30s timeout. If the timeout is exceeded, proctor logs an error and continues (does not abort).

## Output

### Log format

Each line of stdout and stderr from every process is prefixed with the process name and piped to proctor's own stdout:

```
      api | listening on :8080
   worker | connected to redis
 frontend | compiled successfully
```

### Colour

Each process is assigned a colour deterministically from a hash of its name, drawn from the 256-colour ANSI palette (excluding colours that are too close to black or white for terminal readability). The prefix (name + `|`) is coloured; the log line itself is not.

### Alignment

Process name prefixes are right-aligned (left-padded with spaces) to the length of the longest name in the file so that the `|` delimiters align vertically:

```
     init | running migrations
    redis | ready to accept connections
      api | listening on :8080
 frontend | compiled successfully
```

### Stderr

Stderr lines are prefixed identically but rendered in a dimmed or italic variant of the process colour, so they are visually distinct without breaking alignment.

## File Format Summary

```
# Setup
init: just db init
migrate after=init: just db migrate

# Infrastructure
redis: redis-server
postgres ready=tcp://5432: docker run --rm -p 5432:5432 postgres:16

# Services
api **/*.go !**_test.go after=postgres debounce=500ms: \
  CGO_ENABLED=0 go run ./cmd/api
worker **/*.go !**_test.go after=redis ready=tcp://6379: \
  go run ./cmd/worker
frontend web/**/*.{ts,tsx,css} dir=./web: \
  npm run dev
```

## Non-goals

- Remote process management or deployment.
- Container orchestration (use `docker compose` for that).
- Language-specific build intelligence (use the appropriate build tool in the command).
- Windows support (initial version targets Unix-like systems).

