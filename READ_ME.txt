# Validation Archiver

> **Never lose a working version again.**

During development it's common to:

* Fix one bug while accidentally introducing another.
* Forget to commit after a successful test.
* Overwrite yesterday's working code while experimenting today.

Git is excellent at preserving history—but only after you remember to commit.

**Validation Archiver** automatically preserves successful development milestones. When your program completes successfully, it records a snapshot of the project along with execution metadata, making it easy to recover a known-good state without interrupting your workflow.

---

## Why I Built This

I wanted a tool that rewarded successful development instead of relying on my memory.

Instead of thinking:

> "I should commit before I try this..."

I wanted my computer to think:

> "That test passed. This version is worth keeping."

Validation Archiver fills the gap between manual Git commits and full Continuous Integration (CI) pipelines by automatically recording successful local development milestones.

---

# How It Works

```text
Edit code
     │
     ▼
Run Validation Archiver
     │
     ▼
Program executes
     │
     ▼
Validation succeeds
     │
     ▼
Archive changed files
     │
     ├────────► Save metadata
     ├────────► Save stdout/stderr
     ├────────► Record execution context
     │
     ▼
(Optional)
Create Git commit
     │
     ▼
(Optional)
Push to GitHub
```

---

# Features

* ✅ Automatically preserve successful development milestones.
* ✅ Archive only files that actually changed (SHA-256 deduplication).
* ✅ Atomic snapshots to avoid partially-written archives.
* ✅ Capture execution metadata for reproducible development history.
* ✅ Archive entire projects while respecting `.vaignore`.
* ✅ Optional Git commit and push after successful validation.
* ✅ Automatic backup rotation to control disk usage.
* ✅ Designed for solo developers, researchers, and experimentation-heavy workflows.

---

# Installation

```bash
cargo install --path .
```

---

# Basic Usage

## Run a single script

```bash
validation_archiver run python3 hello.py
```

If the program succeeds, the script is archived automatically.

---

## Watch an entire project

```bash
validation_archiver run python3 main.py --watch .
```

Validation Archiver will compare the project folder against the last archive made, backing up every monitored file that changed.

---

## Enable Git integration

```bash
validation_archiver run python3 main.py --watch . --push
```

After a successful validation:

1. Archive the project.
2. Create a Git commit.
3. Push to the configured remote repository.

---

# Project Management

## List tracked projects

```bash
validation_archiver list
```

Displays every archived project currently managed.

---

## Remove a project

```bash
validation_archiver rm MyProject
```

Deletes all archived snapshots for the selected project after confirmation.

---

## Prune old backups

```bash
validation_archiver prune MyProject --max-backups 100
```

Keeps only the newest backups for every monitored file.

---

# Archive Contents

Each archived milestone contains more than just your source code.

Typical archive:

```text
~/.validation_archiver/

MyProject/

main.rs/

1749156821/
    main.rs
    metadata.json
    stdout.log
    stderr.log
    .hash
```

This makes every archived milestone reproducible by preserving:

* the executed command
* program arguments
* working directory
* exit code
* captured stdout
* captured stderr
* timestamp

---

# Current Status

**Project Status:** Beta

Validation Archiver is currently used during development on real projects and has been tested on Linux and Termux.

The archive format and command-line interface may continue to evolve before the first stable (1.0) release.

---

# Roadmap

Current development priorities include:

* GitHub Actions integration
* Automated release workflows
* Additional language validators
* Configurable validation policies
* Expanded project management features

---

# Contributing

Validation Archiver is currently in an incubation phase.

Bug reports, feature requests, and discussion are welcome.

At this stage I am intentionally keeping design decisions centralized while the architecture matures, so pull requests are not currently being accepted.

---

# License

MIT License © 2026 Lou Crawford
