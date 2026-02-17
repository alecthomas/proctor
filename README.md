# A Procfile-compatible process manager with hot reload, readiness probes, dependencies, and more

## Overview

Proctor is a local development process manager that is compatible with and extends the Procfile format. It aims to replace ad-hoc combinations of `foreman`, `watchexec`, and shell scripts with a single declarative file.

![Proctor demo](demo/demo.svg)

**Features:**

- Procfile-compatible syntax with shell-style quoting and line continuation
- One-shot processes (run to completion) and long-running processes
- File watching with glob patterns and exclusions (`.gitignore` respected automatically)
- Hot reload on file changes with configurable debounce and signal
- Process dependencies with `after=` for ordered startup
- Readiness probes (TCP port, HTTP endpoint, or shell command)
- Automatic restart on crash with exponential backoff
- Graceful shutdown in reverse dependency order
- Global and per-process environment variables
- Per-process working directory
- Coloured, aligned log output with optional timestamps
- Multiline command blocks

## Example

```procfile
# Long-running infrastructure with TCP readiness probe
postgres ready=5432: docker run --rm -p 5432:5432 postgres:16

# One-shot: run migrations before anything else (multiline command block)
migrate! **/*.sql dir=./db after=postgres:
    echo "Running migrations..."
    psql -f schema.sql
    psql -f seeds.sql

# One-shot with file watching: re-run codegen when schema changes
codegen! **/*.graphql debounce=1s: npm run codegen

# Long-running with HTTP readiness probe, depends on postgres
api **/*.go !**_test.go after=migrate,postgres ready=http:8080/health
    debounce=1s signal=INT shutdown=10s:
  LOG_LEVEL=debug go run ./cmd/api

# Long-running in subdirectory, watches multiple file types and restarts if codegen is re-run
frontend web/**/*.{ts,tsx,css,html} !web/dist/** dir=./web after=api,codegen: \
  NODE_ENV=development npm run dev
```

## Installation

```sh
curl -fsSL https://raw.githubusercontent.com/alecthomas/proctor/master/install.sh | sh
```

To install a specific version or to a custom directory:

```sh
curl -fsSL https://raw.githubusercontent.com/alecthomas/proctor/master/install.sh | INSTALL_DIR=~/.local/bin sh -s v0.1.0
```

## Environment Variables

| Variable        | Description                                      |
|-----------------|--------------------------------------------------|
| `PROCTOR_FLAGS` | Default flags (shell-quoted), e.g. `-dt --check` |

## Procfile Syntax

### Line format

```
<proc>[!] [!]<pattern>... [option=value ...]: [ENV=VALUE ...] <command>
```

Each line defines a process. The colon (`:`) separates the **declaration** (left) from the **execution** (right). Tokenisation uses shell-style rules throughout: bare words, `'single quoted'` (literal), and `"double quoted"` (with escape sequences).

### Comments and blank lines

Lines starting with `#` are comments. Blank lines are ignored.

### Global environment variables

Lines matching `KEY=VALUE` (with no colon) define global environment variables that are set for all processes:

```procfile
CGO_ENABLED=0
NODE_ENV=development

api: go run ./cmd/api
frontend: npm run dev
```

Values can be bare, single-quoted (literal), or double-quoted (with escape sequences like `\n`, `\t`):

```procfile
SIMPLE=value
SPACES='hello world'
NEWLINE="line1\nline2"
```

Global variables are merged with the inherited environment. Inline `ENV=value` in the command takes precedence over global variables.

### Line continuation

A trailing `\` continues the command onto the next line, following shell conventions:

```
api **/*.go: go run \
  -tags dev \
  ./cmd/api
```

### Multiline command blocks

If the colon is immediately followed by a newline, subsequent indented lines form the command:

```
build!:
    echo "Building..."
    go build -o bin/app ./cmd/app
    echo "Done"
```

Common leading indentation is stripped. The block ends at the first non-indented line (or end of file).

## Declaration (left of `:`)

### Process name

The first token is always the process name. It must be unique within the file. Valid characters: `[a-zA-Z0-9_-]`.

A trailing `!` marks the process as **one-shot** (expected to run to completion and exit). Without the `!`, processes are assumed to be **long-running** (expected to stay alive).

```
migrate!: just db migrate    # one-shot: ready when it exits 0
api: go run ./cmd/api        # long-running: ready immediately on start
```

### Watch patterns

Any token after the process name that is not an option (`key=value`) is interpreted as a file watch pattern. Patterns follow standard globbing rules including `**` for recursive matching and `{a,b}` for alternation. Bare file names are also supported.

A token prefixed with `!` is an exclusion pattern.

```
api **/*.go !**_test.go !vendor/**:
echo Procfile: echo "Procfile changed"
```

If no watch patterns are present, the process is not file-watched.

Paths matching `.gitignore` rules (and the `.git/` directory) are automatically excluded from file watching. This means you don't need to manually add exclusions for directories like `node_modules/`, `target/`, `dist/`, etc.

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

**Note on one-shot processes:** The `ready` option is not permitted for one-shot processes since they become ready when they exit 0. All other options are valid, including watch patterns (to re-run the one-shot when files change).

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

Processes are classified by the `!` suffix on their name:

- **One-shot** (`name!`): Expected to run to completion and exit. Becomes ready when it exits with code 0.
- **Long-running** (`name`): Expected to stay alive. Becomes ready immediately on start (unless a `ready` probe is specified).

### Startup order

1. Parse the Procfile and build a dependency graph from `after=` declarations.
2. Start all processes with no dependencies concurrently.
3. As each process becomes ready (exits 0 for one-shot, passes readiness probe or starts for long-running), start any processes that were waiting on it.

If a one-shot process exits non-zero, startup is aborted and all running processes are shut down. Circular dependencies are detected at parse time and treated as an error.

### Hot Reload

When a file change matches a process's glob patterns (after exclusions):

1. Debounce: wait for the configured debounce interval with no further matching changes.
2. For running processes: send the configured signal (default `SIGTERM`) to the process group, wait up to `shutdown` duration, then `SIGKILL` if needed.
3. For completed one-shot processes: start immediately.
4. Restart the process.
5. All downstream dependents (processes with `after=` pointing to the reloaded process, transitively) are also restarted, in dependency order, after the upstream process becomes ready.

### Restart on crash

If a long-running process exits unexpectedly (not due to a reload or shutdown):

- Restart immediately on first failure.
- Apply exponential backoff on consecutive failures: 1s, 2s, 4s, 8s, 16s, capped at 32s.
- Gradually reset the backoff level while running stably (decrease one level after running for the current backoff duration).
- Log each restart with the exit code or signal.

### Shutdown

On `SIGINT` or `SIGTERM` to proctor itself:

1. Send `SIGTERM` to all process groups in reverse dependency order.
2. Wait up to each process's `shutdown` duration.
3. `SIGKILL` any remaining processes.
4. Exit.

## Readiness

The `ready` option defines how proctor determines a process is ready (for `after=` dependents).

| Format                            | Behaviour                                                              |
|-----------------------------------|------------------------------------------------------------------------|
| `<port>`                          | Poll `localhost:<port>` until a TCP connection succeeds                |
| `http:<port>[/<path>][=<status>]` | Poll `http://localhost:<port>[/<path>]` for the expected status code   |
| `exec:<command>`                  | Run `<command>` via shell; ready when it exits 0                       |

If no `=<status>` is specified for HTTP probes, any non-5xx response is accepted. If `=<status>` is specified, only that exact status code is accepted.

With no `ready` option: long-running processes are ready immediately on start; one-shot processes are ready on exit 0.

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
# Setup (one-shot tasks)
init!: just db init
migrate! after=init: just db migrate

# Infrastructure
redis: redis-server
postgres ready=5432: docker run --rm -p 5432:5432 postgres:16

# Services
api **/*.go !**_test.go after=postgres debounce=500ms: \
  CGO_ENABLED=0 go run ./cmd/api
worker **/*.go !**_test.go after=redis ready=6379: \
  go run ./cmd/worker
frontend web/**/*.{ts,tsx,css} dir=./web: \
  npm run dev
```

## Non-goals

- Remote process management or deployment.
- Container orchestration (use `docker compose` for that).
- Language-specific build intelligence (use the appropriate build tool in the command).
- Windows support (initial version targets Unix-like systems).
