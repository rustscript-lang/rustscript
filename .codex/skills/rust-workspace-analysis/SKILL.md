---
name: rust-workspace-analysis
description: Generate a top-down analysis of a Rust Cargo workspace, including crate-level LOC, cumulative LOC, feature or functionality buckets, Cargo feature flags, and detected test suites. Use when the user asks for repo analysis, crate breakdowns, code lines by feature, cumulative line counts, or test inventory for a Rust repository.
---

# Rust Workspace Analysis

## Overview

Generate a markdown report for a Rust Cargo workspace that breaks the repo down by crate, feature or functionality bucket, and test suite. Use the bundled script instead of rebuilding the counting logic in-chat.

## Workflow

1. Confirm the repo root contains `Cargo.toml` for a Cargo workspace or crate.
2. Run `scripts/generate_repo_analysis.ps1` from the target repo root.
3. Use the default output path `repo-analysis.md` unless the user asks for another location.
4. Summarize the biggest code concentrations in the reply and link the generated report file.

## What The Script Produces

- `Crate Summary`: crate LOC, cumulative LOC, detected tests, Cargo feature flags
- `Crate Feature Matrix`: one row per crate and feature or functionality bucket
- `Crate Test Matrix`: one row per crate and test suite
- Per-crate detail tables for features and tests

## Counting Rules

- Count physical Rust lines in `src/**/*.rs` plus `build.rs`.
- Exclude `tests/`, `examples/`, `docs/`, `target/`, and web assets from LOC totals.
- Treat "feature or functionality" as source or module buckets, not Cargo feature flags.
- List Cargo feature flags separately from code buckets.
- Detect tests from `#[test]` and `#[tokio::test]` in `src/**/*.rs`, `tests/**/*.rs`, and `build.rs`.

## Running The Script

From the repo root:

```powershell
powershell -ExecutionPolicy Bypass -File .codex/skills/rust-workspace-analysis/scripts/generate_repo_analysis.ps1
```

Write to a custom location:

```powershell
powershell -ExecutionPolicy Bypass -File .codex/skills/rust-workspace-analysis/scripts/generate_repo_analysis.ps1 -OutputPath target/repo-analysis.md
```

Analyze another repo path explicitly:

```powershell
powershell -ExecutionPolicy Bypass -File .codex/skills/rust-workspace-analysis/scripts/generate_repo_analysis.ps1 -RepoRoot D:\path\to\repo
```

## Response Pattern

- Mention the report path.
- State the total production LOC and detected tests.
- Call out the largest crates and largest feature or functionality buckets.
- If the user asks for a table change, re-run the script after updating it rather than hand-editing stale output.

## Resources

### scripts/

- `generate_repo_analysis.ps1`: generates the markdown report deterministically for Cargo workspaces.
