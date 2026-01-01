---
name: dependency-mapping
description: Identifies dependencies between GitHub issues and creates appropriate cross-references. Use when you need to map issue relationships, find blockers, or organize work order based on dependencies.
---

# Issue Dependency Mapping

## Purpose

This skill analyzes GitHub issues to:
1. Identify issues that depend on other issues
2. Find issues that block other work
3. Create cross-references between related issues
4. Build a dependency graph for planning

## Instructions

### Step 1: Fetch All Open Issues

```bash
gh issue list --state open --json number,title,body,labels,comments --limit 100
```

### Step 2: Analyze for Dependencies

Look for these dependency types:

**Explicit Dependencies:**
- Mentions of other issue numbers (#N)
- Phrases like "depends on", "blocked by", "requires", "after"
- References to PRs or commits

**Implicit Dependencies:**
- Issues touching the same files/modules
- Feature requests that build on other features
- Bug fixes that require refactoring first
- Issues in the same functional area

**Technical Dependencies:**
- Database schema changes before feature work
- API changes before client updates
- Infrastructure before application features
- Core functionality before extensions

### Step 3: Categorize Relationships

| Relationship | Description | Example |
|--------------|-------------|---------|
| **blocks** | Must complete A before starting B | Refactoring blocks new feature |
| **related-to** | Same area but independent | Two UI improvements |
| **duplicates** | Same issue reported twice | Duplicate bug reports |
| **parent-of** | Epic/umbrella issue | Feature epic with sub-tasks |

### Step 4: Update Issues with References

**Add blocking reference:**
```bash
gh issue comment <blocked-issue> --body "## Dependency Note

This issue is blocked by #<blocking-issue>.

**Reason:** [Why this dependency exists]

This will be ready for implementation once #<blocking-issue> is resolved."
```

**Add related reference:**
```bash
gh issue comment <issue> --body "## Related Issues

This issue is related to:
- #<related-1> - [brief description of relationship]
- #<related-2> - [brief description of relationship]

Consider coordinating these changes together."
```

**Mark duplicates:**
```bash
gh issue comment <duplicate> --body "This appears to be a duplicate of #<original>. Closing in favor of that issue."
gh issue close <duplicate> --reason "not planned" --comment "Duplicate of #<original>"
```

### Step 5: Generate Dependency Report

Create a comprehensive dependency map:

```markdown
## Issue Dependency Map

### Dependency Graph

```
#15 (Database schema update)
  └── #18 (User authentication)
      └── #22 (User preferences)
  └── #20 (API v2 endpoints)

#12 (Refactor job system)
  └── #25 (Parallel builds)
  └── #27 (Build caching)
```

### Blocking Issues (High Priority)
These issues block other work and should be prioritized:

| Issue | Blocks | Description |
|-------|--------|-------------|
| #15 | #18, #20 | Database schema update |
| #12 | #25, #27 | Job system refactor |

### Ready to Start
These issues have no blockers:
- #10 - Fix typo in README
- #11 - Add --help examples
- #15 - Database schema update

### Waiting on Dependencies
These issues are blocked:
- #18 - Waiting on #15
- #25 - Waiting on #12

### Related Issue Clusters
Issues that should be considered together:

**Cluster: Build Performance**
- #12 - Refactor job system
- #25 - Parallel builds
- #27 - Build caching

**Cluster: Documentation**
- #30 - Update README
- #31 - Add API docs
- #32 - Tutorial improvements

### Suggested Work Order
Based on dependencies, here's the recommended sequence:

1. **Phase 1 (No blockers):**
   - #10, #11, #15, #12

2. **Phase 2 (After Phase 1):**
   - #18, #20, #25, #27

3. **Phase 3 (After Phase 2):**
   - #22
```

## Guidelines

- Only mark issues as blocked when there's a real technical dependency
- Be specific about why dependencies exist
- Don't create circular dependencies
- Consider partial dependencies (can start work but not complete)
- Check for implicit dependencies through code analysis
- Update dependency notes when relationships change

## Detecting Dependencies Through Code

When issues reference the same code areas, investigate further:

1. **Find files mentioned in issues:**
   ```bash
   # Extract file paths from issue body
   gh issue view <number> --json body
   ```

2. **Check for overlapping changes:**
   - If Issue A modifies `src/job_system.rs`
   - And Issue B adds features to `src/job_system.rs`
   - They may need coordination (not necessarily blocking)

3. **Identify shared abstractions:**
   - Changes to traits/interfaces
   - Database schema modifications
   - API contract changes

## Example Dependency Analysis

**Issue #18: Add user authentication**
```
Analysis:
- References database user table (#15)
- Mentions API endpoints (#20)
- Touches src/auth.rs, src/api.rs

Dependencies found:
- BLOCKED BY #15 (need user table first)
- RELATED TO #20 (both touch API layer)
```

**Resulting comments:**

On #18:
> ## Dependency Note
>
> This issue is blocked by #15 (Database schema update).
>
> **Reason:** User authentication requires the users table which will be created in #15.
>
> Additionally related to #20 (API v2 endpoints) as both modify the API layer.

On #15:
> ## Note
>
> This issue blocks:
> - #18 (User authentication) - needs users table
> - #20 (API v2 endpoints) - needs updated schema
>
> Please prioritize this issue to unblock dependent work.
