---
name: issue-review
description: Reviews and categorizes GitHub issues by difficulty (easy/medium/hard). Use when you need to triage open issues, ask clarifying questions for incomplete issues, or write implementation plans for ready issues.
---

# Issue Review and Categorization

## Project Board

All issues are tracked on the **Anubis Issue Tracker** project board:
- **Project URL:** https://github.com/users/forrestthewoods/projects/8
- **Project Number:** 8
- **Owner:** forrestthewoods

## Board Status Workflow

The project board uses these status columns:

| Status | Description |
|--------|-------------|
| **Triage** | New issues that haven't been reviewed yet |
| **Waiting for Comment** | Issues waiting for a response/clarification |
| **Ready to Plan** | Issues with all needed information, ready for implementation planning |
| **Ready to Implement** | Issues with a plan ready to be worked on |
| **In-progress** | Issues under active development |
| **Ready to Review** | Work done, ready for review and merge |
| **Done** | Closed and completed |

## Purpose

This skill helps triage GitHub issues by:
1. Reviewing all open issues in the repository
2. Categorizing each by difficulty (easy, medium, hard)
3. Moving issues through the appropriate board statuses
4. Identifying issues that need more detail and posting clarifying questions
5. Writing implementation plans for well-defined issues

## Instructions

When invoked, follow these steps:

### Step 1: Fetch Open Issues

Use the GitHub CLI to list all open issues:

```bash
gh issue list --state open --json number,title,body,labels,comments --limit 100
```

### Step 2: Analyze Each Issue

For each issue, evaluate:

**Difficulty Assessment Criteria:**

| Difficulty | Criteria |
|------------|----------|
| **Easy** | Single file change, clear requirements, isolated scope, minimal testing needed, good first issue |
| **Medium** | Multiple files, some design decisions, moderate testing, touches 1-2 modules |
| **Hard** | Architectural changes, complex logic, extensive testing, cross-cutting concerns, unclear requirements needing investigation |

**Completeness Check:**
- Does it have clear acceptance criteria?
- Are reproduction steps provided (for bugs)?
- Is the scope well-defined?
- Are there conflicting requirements?

### Step 3: Take Action Based on Analysis

**For incomplete issues** (missing critical details):

1. Post a comment asking clarifying questions:
```bash
gh issue comment <number> --body "## Clarification Needed

Thank you for opening this issue. To help prioritize and plan implementation, could you please clarify:

1. [Specific question about requirements]
2. [Question about expected behavior]
3. [Question about scope/constraints]

Once we have these details, we can create an implementation plan."
```

2. Move to "Waiting for Comment" status on the project board

**For issues with enough information but no plan** (ready to plan):

1. Add a difficulty label:
```bash
gh issue edit <number> --add-label "difficulty: easy|medium|hard"
```

2. Move to "Ready to Plan" status on the project board

**For well-defined issues** (ready for implementation plan):

1. Add a difficulty label if not present:
```bash
gh issue edit <number> --add-label "difficulty: easy|medium|hard"
```

2. Post an implementation plan as a comment:
```bash
gh issue comment <number> --body "## Implementation Plan

**Difficulty:** [easy|medium|hard]

### Overview
[Brief description of the approach]

### Steps
1. [First implementation step]
2. [Second implementation step]
...

### Files to Modify
- \`path/to/file.rs\` - [what changes]
- \`path/to/another.rs\` - [what changes]

### Testing
- [Test case 1]
- [Test case 2]

### Considerations
- [Any edge cases or concerns]"
```

3. Move to "Ready to Implement" status on the project board

### Step 4: Update Project Board

Use the GitHub CLI to update issue status on project #8:

```bash
# Get project item ID for an issue
gh project item-list 8 --owner forrestthewoods --format json | jq '.items[] | select(.content.number == <issue-number>)'

# Update status (requires project item ID and field/option IDs)
gh project item-edit --project-id <project-id> --id <item-id> --field-id <status-field-id> --single-select-option-id <option-id>
```

### Step 5: Generate Summary Report

After processing all issues, provide a summary:

```
## Issue Triage Summary

### By Status
- Triage: #1, #2
- Waiting for Comment: #4, #9
- Ready to Plan: #5, #7
- Ready to Implement: #3, #8, #12

### By Difficulty
- Easy: #1, #5, #12
- Medium: #3, #7
- Hard: #2, #8

### Actions Taken
- Requested clarification: #4, #9
- Added implementation plans: #3, #8, #12
- Labeled: #1, #2, #3, #5, #7, #8, #12
```

## Guidelines

- Be respectful and constructive in all comments
- Provide specific, actionable questions when asking for clarification
- Implementation plans should be detailed enough for another developer to follow
- Consider existing architecture when estimating difficulty
- Look at related code before categorizing
- Check if issues are duplicates or related to existing work
- If unsure about difficulty, err on the side of marking as harder
- Always update the project board status after taking action

## Example Clarifying Questions

For a bug report:
- "What version of Anubis are you using?"
- "Can you share the exact error message?"
- "What operating system are you on?"
- "Can you share a minimal reproduction case?"

For a feature request:
- "What problem does this solve for you?"
- "Are there any constraints we should consider?"
- "How should this interact with [existing feature]?"
- "What's the expected behavior when [edge case]?"

## Example Implementation Plan

```markdown
## Implementation Plan

**Difficulty:** medium

### Overview
Add a `--dry-run` flag to the build command that shows what would be compiled without actually running the build.

### Steps
1. Add `dry_run: bool` field to BuildArgs in main.rs
2. Pass dry_run flag through to Anubis::build_single_target
3. Modify job_system.rs to support a dry-run mode
4. Update CcBinary and CcStaticLibrary to check dry_run flag
5. Print planned actions instead of executing when dry_run is true

### Files to Modify
- `src/main.rs` - Add CLI argument
- `src/anubis.rs` - Pass through flag
- `src/job_system.rs` - Add dry_run support
- `src/cc_rules.rs` - Check flag before execution

### Testing
- Test dry-run with simple_cpp example
- Verify no files are created in dry-run mode
- Check output shows planned compilation steps

### Considerations
- Dependency resolution should still happen
- Output format should be human-readable
```
