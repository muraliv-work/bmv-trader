---
name: review
description: Pre-commit review for bmv-trader. Runs the project's reviewer agents in parallel on the current changes, merges and ranks findings, and gives a commit verdict. Use when the user says "review", "/review", or before committing.
---

# /review

1. **Scope.** Collect what changed: `git status --short`, `git diff`, `git diff --staged`, plus untracked files (on a repo with no commits, everything tracked or untracked except gitignored paths is in scope). If nothing changed, say so and stop.
2. **Pick reviewers** (from `.claude/agents/`):
   - always: `code-reviewer`, `rust-reviewer`
   - if the change touches `config`, `broker`, `risk`, `vetting`, `llm`, `.env*`, `.gitignore`, or `Cargo.toml`: also `security-reviewer`
   - if it touches error handling, network, I/O, or engine/broker code: also `silent-failure-hunter`
   - if it adds or changes public types/structs/enums: also `type-design-analyzer`
3. **Run them in parallel**, one Agent call each in a single message. If an agent type isn't registered yet (new this session), use `general-purpose` and tell it to first read `.claude/agents/<name>.md` and adopt it. Brief each with: the file list in scope, "read-only, do not edit, do not call Alpaca/Anthropic/OmniRoute", `export PATH=/opt/homebrew/opt/rustup/bin:$PATH` for cargo, and that `docs/DESIGN.md` §3, §9, §16 define the rules. Do not pass your own opinions of the code.
4. **Merge.** Deduplicate findings across reviewers, keep the most specific file:line, rank CRITICAL → HIGH → MEDIUM. Drop anything you can disprove by reading the code; mark findings two or more reviewers agree on.
5. **Verdict.** BLOCK if any CRITICAL survives; FIX FIRST if HIGH; otherwise OK TO COMMIT. List what each reviewer covered and what nobody checked.

Do not fix anything during the review; offer the fixes afterwards.
