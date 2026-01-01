---
name: branch-naming
description: Defines branch naming conventions for issue work. Use when creating branches for issues, understanding work status from branches, or linking commits to issues.
---

# Branch Naming Conventions

## Purpose

This skill establishes consistent branch naming conventions that:
1. Clearly link branches to GitHub issues
2. Enable automated detection of work status
3. Provide context about the type of work being done
4. Support the issue-review skill in determining issue states

## Branch Naming Format

### Standard Pattern

```
<prefix>/<issue-number>-<short-description>
```

**Examples:**
- `feature/42-add-build-caching`
- `fix/15-resolve-linker-error`
- `refactor/28-simplify-job-system`

### Prefixes

| Prefix | Use Case | Example |
|--------|----------|---------|
| `feature/` | New functionality | `feature/42-add-parallel-builds` |
| `fix/` | Bug fixes | `fix/15-null-pointer-crash` |
| `refactor/` | Code restructuring | `refactor/28-extract-parser-module` |
| `docs/` | Documentation changes | `docs/33-update-readme` |
| `test/` | Test additions/fixes | `test/45-add-integration-tests` |
| `chore/` | Maintenance tasks | `chore/50-update-dependencies` |

### Claude-Specific Branches

When Claude Code creates branches, use this pattern:

```
claude/<prefix>-<issue-number>-<short-slug>-<session-id>
```

**Examples:**
- `claude/feature-42-build-caching-Abc12`
- `claude/fix-15-linker-error-Xyz99`

The session ID suffix ensures uniqueness across multiple Claude sessions.

## Naming Rules

### DO:
- Include the issue number immediately after the prefix
- Use lowercase letters and hyphens only
- Keep descriptions short (2-4 words)
- Use descriptive slugs that summarize the work

### DON'T:
- Use spaces or underscores
- Include special characters (except hyphens)
- Create overly long branch names (max ~50 chars)
- Omit the issue number for issue-related work

### Description Guidelines

| Good | Bad | Why |
|------|-----|-----|
| `42-add-caching` | `42` | Too vague |
| `15-fix-null-ptr` | `15-fix-the-bug-where-null-pointer-exception-occurs` | Too long |
| `28-refactor-parser` | `28-misc-changes` | Not descriptive |

## Commit Message Conventions

### Format

```
<type>: <short description>

<optional body>

Refs #<issue-number>
```

Or when completing work:

```
<type>: <short description>

<optional body>

Fixes #<issue-number>
```

### Types

| Type | Description |
|------|-------------|
| `feat` | New feature |
| `fix` | Bug fix |
| `refactor` | Code restructuring |
| `docs` | Documentation |
| `test` | Test changes |
| `chore` | Maintenance |

### Keywords for Issue Linking

| Keyword | Effect | Use When |
|---------|--------|----------|
| `Fixes #N` | Closes issue when PR merges | Work completely resolves the issue |
| `Closes #N` | Closes issue when PR merges | Same as Fixes |
| `Resolves #N` | Closes issue when PR merges | Same as Fixes |
| `Refs #N` | Links without closing | Work is partial or related |
| `Part of #N` | Links without closing | Issue requires multiple PRs |

### Examples

**Feature commit:**
```
feat: add build result caching

Implement file-based caching for compilation results.
Cache key includes source hash and compiler flags.

Fixes #42
```

**Partial work commit:**
```
refactor: extract cache module

Move caching logic to dedicated module in preparation
for adding build caching feature.

Refs #42
```

**Bug fix commit:**
```
fix: handle null toolchain path

Add validation for toolchain path before use.
Prevents crash when toolchain is not configured.

Fixes #15
```

## Detecting Work Status from Branches

The issue-review skill can determine issue status by analyzing branches:

### Branch Detection Rules

```bash
# Find branches for a specific issue
git branch -a | grep -E "(^|/)([0-9]+)-" | grep "<issue-number>"

# Or using GitHub CLI
gh pr list --search "head:<issue-number>" --state all --json number,state,isDraft,headRefName
```

### Status Inference

| Branch State | PR State | Inferred Issue Status |
|--------------|----------|----------------------|
| Branch exists, no PR | - | **In-progress** |
| Branch exists, draft PR | Draft | **In-progress** |
| Branch exists, open PR | Open (ready) | **Ready to Review** |
| Branch merged | PR Merged | **Done** |
| Branch deleted, PR closed | Closed (not merged) | Work abandoned, check issue |

### Example Detection Script

```bash
#!/bin/bash
ISSUE_NUMBER=$1

# Check for branches matching issue
BRANCHES=$(git branch -a --list "*${ISSUE_NUMBER}-*" 2>/dev/null)

# Check for PRs
PR_INFO=$(gh pr list --search "head:${ISSUE_NUMBER}" --json number,state,isDraft,headRefName 2>/dev/null)

if [ -n "$PR_INFO" ] && [ "$PR_INFO" != "[]" ]; then
    IS_DRAFT=$(echo "$PR_INFO" | jq -r '.[0].isDraft')
    STATE=$(echo "$PR_INFO" | jq -r '.[0].state')

    if [ "$STATE" = "MERGED" ]; then
        echo "Status: Done"
    elif [ "$IS_DRAFT" = "true" ]; then
        echo "Status: In-progress (draft PR)"
    else
        echo "Status: Ready to Review"
    fi
elif [ -n "$BRANCHES" ]; then
    echo "Status: In-progress (branch exists, no PR)"
else
    echo "Status: No active work detected"
fi
```

## PR Title Conventions

When creating PRs, use this format:

```
<type>: <description> (#<issue-number>)
```

**Examples:**
- `feat: Add build caching (#42)`
- `fix: Resolve linker error on Windows (#15)`
- `refactor: Simplify job system architecture (#28)`

## Workflow Integration

### Starting Work on an Issue

1. **Create branch with proper naming:**
   ```bash
   git checkout -b feature/42-add-build-caching
   ```

2. **Make commits with issue references:**
   ```bash
   git commit -m "feat: implement cache storage

   Refs #42"
   ```

3. **Create PR with proper title:**
   ```bash
   gh pr create --title "feat: Add build caching (#42)" \
     --body "## Summary
   Implements build result caching.

   Fixes #42"
   ```

### Issue-Review Integration

The issue-review skill can use branch/PR information to:

1. **Detect work in progress:**
   - Search for branches containing issue numbers
   - Check for draft PRs linked to issues

2. **Verify status accuracy:**
   - Issue marked "Ready to Review" should have an open PR
   - Issue marked "In-progress" should have a branch or draft PR
   - Issue marked "Done" should have a merged PR

3. **Identify stale work:**
   - Branches with no commits in 14+ days
   - Draft PRs with no updates in 7+ days

## Quick Reference

### Branch Naming Cheat Sheet

```
feature/<issue>-<slug>    # New features
fix/<issue>-<slug>        # Bug fixes
refactor/<issue>-<slug>   # Restructuring
docs/<issue>-<slug>       # Documentation
test/<issue>-<slug>       # Test changes
chore/<issue>-<slug>      # Maintenance

# Claude sessions
claude/<type>-<issue>-<slug>-<session>
```

### Commit Keywords

```
Fixes #N      # Closes issue on merge
Closes #N     # Closes issue on merge
Resolves #N   # Closes issue on merge
Refs #N       # Links without closing
Part of #N    # Partial work
```

### Status Detection Summary

```
Branch only           → In-progress
Branch + Draft PR     → In-progress
Branch + Open PR      → Ready to Review
Merged PR             → Done
No branch/PR          → Check issue status
```

## Guidelines

- Always include issue numbers in branch names for trackable work
- Use consistent prefixes across the team
- Keep branch names concise but descriptive
- Reference issues in every commit for traceability
- Use closing keywords only when work fully resolves the issue
- Delete branches after PR merge to keep repository clean
- The issue-review skill relies on these conventions for accurate status detection
