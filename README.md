<p align="center">
  <h1 align="center">AgentFS</h1>
</p>

<p align="center">
  The filesystem for agents.
</p>

<p align="center">
  <a title="Build Status" target="_blank" href="https://github.com/tursodatabase/agentfs/actions/workflows/rust.yml"><img src="https://img.shields.io/github/actions/workflow/status/tursodatabase/agentfs/rust.yml?style=flat-square"></a>
  <a title="Rust" target="_blank" href="https://crates.io/crates/agentfs-sdk"><img alt="Crate" src="https://img.shields.io/crates/v/agentfs-sdk"></a>
  <a title="MIT" target="_blank" href="https://github.com/tursodatabase/agentfs/blob/main/LICENSE.md"><img src="http://img.shields.io/badge/license-MIT-orange.svg?style=flat-square"></a>
</p>
<p align="center">
  <a title="Users's Discord" target="_blank" href="https://tur.so/discord"><img alt="Chat with other users of Turso (and Turso Cloud) on Discord" src="https://img.shields.io/discord/933071162680958986?label=Discord&logo=Discord&style=social&label=Users"></a>
</p>

---

> **⚠️ Warning:** This software is in BETA. It may still contain bugs and unexpected behavior. Use caution with production data and ensure you have backups.

## 🎯 What is AgentFS?

AgentFS is a filesystem explicitly designed for AI agents. Just as traditional filesystems provide file and directory abstractions for applications, AgentFS provides the storage abstractions that AI agents need.

The AgentFS repository consists of the following:

* **SDK** - [Rust](sdk/rust) library for programmatic filesystem access.
* **[CLI](MANUAL.md)** - Command-line interface for managing agent filesystems:
  - Mount AgentFS on host filesystem with FUSE on Linux and NFS on macOS.
  - Access AgentFS files with a command line tool.
* **[AgentFS Specification](SPEC.md)** - SQLite-based agent filesystem specification.

## 💡 Why AgentFS?

AgentFS provides the following benefits for agent state management:

* **Auditability**: Every file operation, tool call, and state change is recorded in a SQLite database file. Query your agent's complete history with SQL to debug issues, analyze behavior, or meet compliance requirements.
* **Reproducibility**: Snapshot an agent's state at any point with cp agent.db snapshot.db. Restore it later to reproduce exact execution states, test what-if scenarios, or roll back mistakes.
* **Portability**: The entire agent runtime—files, state, history —is stored in a single SQLite file. Move it between machines, check it into version control, or deploy it to any system where Turso runs.

Read more about the motivation for AgentFS in the announcement [blog post](https://turso.tech/blog/agentfs).

## 🧑‍💻 Getting Started

### Using the CLI

Install the AgentFS CLI:

```bash
curl -fsSL https://agentfs.ai/install | bash
```

Initialize an agent filesystem:

```bash
$ agentfs init my-agent
Created agent filesystem: .agentfs/my-agent.db
Agent ID: my-agent
```

Inspect the agent filesystem:

```bash
$ agentfs fs my-agent ls
Using agent: my-agent
f hello.txt

$ agentfs fs my-agent cat hello.txt
hello from agent
```

You can also use a database path directly:

```bash
$ agentfs fs .agentfs/my-agent.db cat hello.txt
hello from agent
```

View the agent's action timeline:

```bash
$ agentfs timeline my-agent
ID   TOOL                 STATUS       DURATION STARTED
4    execute_code         pending            -- 2024-01-05 09:44:20
3    api_call             error           300ms 2024-01-05 09:44:15
2    read_file            success          50ms 2024-01-05 09:44:10
1    web_search           success        1200ms 2024-01-05 09:43:45
```

You can mount an agent filesystem using FUSE (Linux) or NFS (macOS):

```bash
$ agentfs mount my-agent ./mnt
$ echo "hello" > ./mnt/hello.txt
$ cat ./mnt/hello.txt
hello
```

You can also run a program in an experimental sandbox with the agent filesystem mounted at `/agent`:

```bash
$ agentfs run /bin/bash
Welcome to AgentFS!

$ echo "hello from agent" > /agent/hello.txt
$ cat /agent/hello.txt
hello from agent
$ exit
```

Read the **[User Manual](MANUAL.md)** for complete documentation.

### Using the SDK

The Rust SDK lives in **[sdk/rust](sdk/rust)** for programmatic filesystem access.

## 🔧 How AgentFS Works?

<img align="right" width="40%" src=".github/assets/agentfs-arch.svg">

AgentFS is an agent filesystem accessible through an SDK that provides three essential interfaces for agent state management:

* **Filesystem:** A POSIX-like filesystem for files and directories
* **Key-Value:** A key-value store for agent state and context
* **Toolcall:** A toolcall audit trail for debugging and analysis

At the heart of AgentFS is the [agent filesystem](SPEC.md), a complete SQLite-based storage system for agents implemented using [Turso](https://github.com/tursodatabase/turso). Everything an agent does—every file it creates, every piece of state it stores, every tool it invokes—lives in a single SQLite database file.

For sandboxed coding-agent workloads, AgentFS can layer that SQLite-backed
filesystem over a read-only host directory. Reads are scoped to the configured
base tree, while writes go only to the AgentFS delta database. The real
filesystem is never modified by copy-on-write operations. On Linux, the FUSE
backend dispatches requests through a bounded worker pool and a read/write lane:
read-heavy operations can run concurrently against internally synchronized
backends, while namespace and data mutations remain serialized at the
filesystem/SQLite transaction boundaries. This preserves AgentFS's two core
safety properties: one portable database contains the virtual filesystem state,
and sandboxed writes do not touch the real filesystem.

## 🤔 FAQ

### How is AgentFS different from _X_?

[Bubblewrap](https://github.com/containers/bubblewrap) provides filesystem isolation using Linux namespaces and overlays. While you could achieve similar isolation with a `bwrap` call that mounts `/` read-only and uses `--tmp-overlay` on the working directory, the key difference is persistence and queryability: with AgentFS, the upper filesystem is stored in a single SQLite database file, which you can query, snapshot, and move to another machine. Read more about the motivation in the announcement [blog post](https://turso.tech/blog/agentfs).

[Docker Sandbox](https://www.docker.com/blog/docker-sandboxes-a-new-approach-for-coding-agent-safety/) and AgentFS are complementary rather than competing. AgentFS answers "what happened and what's the state?" while Docker Sandboxes answer "how do I run this safely?" You could use both together: run an agent inside a Docker Sandbox for security, while using AgentFS inside that sandbox for structured state management and audit trails.

[Git worktrees](https://git-scm.com/docs/git-worktree) let you check out multiple branches of a repository into separate directories, allowing agents to work on independent copies of the source code—similar to AgentFS. But AgentFS solves the problem at a lower level. With git worktrees, nothing prevents an agent from modifying files outside its worktree: another agent's worktree, system files, or anything else on the filesystem. The isolation is purely conventional, not enforced. AgentFS provides filesystem-level copy-on-write isolation that's system-wide and cannot be bypassed—letting you safely run untrusted agents. And because it operates below git, it also handles untracked files, making it useful beyond just version-controlled source code.

### Why implement AgentFS at the filesystem layer instead of using containers or VMs?

The filesystem layer gives us capabilities that block devices can't. First, because everything is stored in structured SQLite tables, you can query the filesystem, which is essential for auditability and debugging agent behavior. Second, SQLite's write-ahead log enables snapshotting and time-travel forking by capturing every filesystem change. Third, we can provide an SDK that works in environments such as serverless or the browser, where there's no way to mount a block device at all. Note that this approach works fine with containers and VMs too—you can use AgentFS via remote filesystem protocols like NFS or through mechanisms like virtio-fuse.

## 📚 Learn More

- **[User Manual](MANUAL.md)** - Complete guide to using the AgentFS CLI and SDK
- **[Agent Filesystem Specification](SPEC.md)** - Technical specification of the agent filesystem SQLite schema
- **[Turso database](https://github.com/tursodatabase/turso)** - an in-process SQL database, compatible with SQLite.

### Blog Posts

- **[Introducing AgentFS](https://turso.tech/blog/agentfs)** - The motivation behind AgentFS
- **[AgentFS with FUSE](https://turso.tech/blog/agentfs-fuse)** - Mounting agent filesystems using FUSE
- **[AgentFS with Overlay Filesystem](https://turso.tech/blog/agentfs-overlay)** - Sandboxing agents with copy-on-write overlays
- **[AI Agents with Just Bash](https://turso.tech/blog/agentfs-just-bash)** - Safe bash command execution for agents
- **[AgentFS in the Browser](https://turso.tech/blog/agentfs_browser)** - Running AgentFS in browsers with WebAssembly
- **[Making Coding Agents Safe Using LlamaIndex](https://www.llamaindex.ai/blog/making-coding-agents-safe-using-llamaindex)** - Using AgentFS with LlamaIndex

## 📝 License

MIT
