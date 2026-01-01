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

This skill ensures the project board accurately reflects the current state of all issues by:
1. Detecting the correct status for each issue based on its actual state
2. Identifying mismatches between current board status and actual state
3. Categorizing issues by difficulty (easy, medium, hard)
4. Posting clarifying questions for incomplete issues
5. Writing implementation plans for well-defined issues

## Instructions

### Step 1: Gather Complete Issue Data

Collect all information needed to determine correct status:

```bash
# Get all open issues with full details
gh issue list --state open --json number,title,body,labels,comments,assignees,state --limit 100

# Get all PRs and their linked issues
gh pr list --state open --json number,title,state,isDraft,body,url --limit 100

# Get current board state
gh project item-list 8 --owner forrestthewoods --format json
```

For each issue, also check for linked PRs:
```bash
# Check if issue has linked PRs (search PR bodies for "Fixes #N", "Closes #N", etc.)
gh pr list --search "in:body fixes:#<number> OR closes:#<number> OR resolves:#<number>" --json number,title,state,isDraft
```

### Step 2: Determine Correct Status for Each Issue

Apply these rules **in order** (first match wins):

#### Rule 1: Has Merged PR → **Done**
```
IF issue has a merged PR that references it
THEN status should be "Done" (and issue should be closed)
```

#### Rule 2: Has Open PR (Ready for Review) → **Ready to Review**
```
IF issue has an open, non-draft PR that references it
THEN status should be "Ready to Review"
```

#### Rule 3: Has Draft PR or Assignee Actively Working → **In-progress**
```
IF issue has a draft PR that references it
OR issue has an assignee AND recent activity indicating active work
THEN status should be "In-progress"
```

#### Rule 4: Has Implementation Plan → **Ready to Implement**
```
IF issue comments contain an "## Implementation Plan" section
AND the plan appears complete (has steps, files to modify)
THEN status should be "Ready to Implement"
```

#### Rule 5: Waiting for User Response → **Waiting for Comment**
```
IF the most recent comment is a question from a maintainer/bot
AND the issue author hasn't responded yet
THEN status should be "Waiting for Comment"
```

#### Rule 6: Has Sufficient Information → **Ready to Plan**
```
IF issue has clear requirements/acceptance criteria
AND issue has enough detail to write an implementation plan
AND no outstanding questions
THEN status should be "Ready to Plan"
```

#### Rule 7: Default → **Triage**
```
IF none of the above apply
THEN status should be "Triage"
```

### Step 3: Analyze Each Issue

For each open issue, determine:

1. **Current Board Status**: What column is it in now?
2. **Correct Status**: Based on the rules above, where should it be?
3. **Status Match**: Do they match? If not, flag for update.
4. **Difficulty**: Easy / Medium / Hard (if not already labeled)
5. **Action Needed**: What action should be taken?

**Difficulty Assessment Criteria:**

| Difficulty | Criteria |
|------------|----------|
| **Easy** | Single file change, clear requirements, isolated scope, minimal testing needed |
| **Medium** | Multiple files, some design decisions, moderate testing, touches 1-2 modules |
| **Hard** | Architectural changes, complex logic, extensive testing, cross-cutting concerns |

**Completeness Check for "Ready to Plan":**
- Does it have clear acceptance criteria?
- Are reproduction steps provided (for bugs)?
- Is the scope well-defined?
- Are there conflicting requirements?

### Step 4: Generate Status Sync Report

Create a comprehensive report showing current vs correct status:

```markdown
## Issue Status Sync Report

### Status Mismatches (Need Update)

| Issue | Title | Current Status | Correct Status | Reason |
|-------|-------|----------------|----------------|--------|
| #25 | Add caching | Ready to Plan | Ready to Review | Has open PR #31 |
| #18 | Fix build | In-progress | Ready to Implement | PR was closed, no active work |
| #12 | New feature | Triage | Waiting for Comment | Questions posted 3 days ago |

### Issues with PRs

| Issue | PR | PR State | Issue Status Should Be |
|-------|-----|----------|------------------------|
| #25 | #31 | Open (ready) | Ready to Review |
| #30 | #35 | Draft | In-progress |
| #22 | #28 | Merged | Done |

### Issues Needing Attention

**Need Clarification (move to Waiting for Comment):**
- #12 - Missing reproduction steps
- #15 - Unclear scope

**Ready for Implementation Plan (move to Ready to Implement after planning):**
- #8 - Has all needed info, needs plan written
- #14 - Clear requirements, ready to plan

**Stale in Waiting for Comment (>7 days):**
- #5 - Asked for details 10 days ago, no response

### By Difficulty

| Difficulty | Count | Issues |
|------------|-------|--------|
| Easy | 5 | #3, #7, #10, #12, #20 |
| Medium | 8 | #4, #8, #11, #14, #15, #18, #22, #25 |
| Hard | 3 | #5, #9, #30 |
| Unlabeled | 2 | #1, #2 |
```

### Step 5: Take Actions

Based on the analysis, take appropriate actions:

**For status mismatches:**
- Report the mismatch and the command needed to fix it
- The board should be updated to reflect actual state

**For issues needing clarification:**
```bash
gh issue comment <number> --body "## Clarification Needed

Thank you for opening this issue. To help prioritize and plan implementation, could you please clarify:

1. [Specific question about requirements]
2. [Question about expected behavior]
3. [Question about scope/constraints]

Once we have these details, we can create an implementation plan."
```

**For issues ready for implementation plans:**
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

### Testing
- [Test case 1]
- [Test case 2]

### Considerations
- [Any edge cases or concerns]"
```

**For adding difficulty labels:**
```bash
gh issue edit <number> --add-label "difficulty: easy"
gh issue edit <number> --add-label "difficulty: medium"
gh issue edit <number> --add-label "difficulty: hard"
```

### Step 6: Generate Final Summary

```markdown
## Issue Review Summary

### Board Sync Status
- **Total Open Issues:** 20
- **Correctly Categorized:** 15
- **Need Status Update:** 5

### Actions Taken
- Posted clarifying questions: #12, #15
- Wrote implementation plans: #8, #14
- Added difficulty labels: #1, #2, #8, #14

### Actions Needed (Board Updates)
These issues need their board status updated:

1. **#25** → Move to "Ready to Review" (has PR #31)
2. **#18** → Move to "Ready to Implement" (PR closed)
3. **#12** → Move to "Waiting for Comment" (questions posted)

### Issues Ready for Work
**Easy (Quick Wins):**
- #3 - Fix typo in error message
- #7 - Update help text

**Medium:**
- #8 - Add --verbose flag (plan written)
- #14 - Improve error handling (plan written)

**Hard:**
- #9 - Refactor job system
```

## Status Detection Examples

### Example 1: Issue with Open PR
```
Issue #25: "Add build caching"
- Current board status: Ready to Plan
- Has PR #31 (open, not draft)
- PR body contains "Fixes #25"

→ Correct status: Ready to Review
→ Action: Update board status
```

### Example 2: Issue with Questions Asked
```
Issue #12: "Support ARM64"
- Current board status: Triage
- Last comment (3 days ago): "Could you clarify which ARM64 targets?"
- No response from author yet

→ Correct status: Waiting for Comment
→ Action: Update board status
```

### Example 3: Issue with Implementation Plan
```
Issue #8: "Add --dry-run flag"
- Current board status: Ready to Plan
- Has comment with "## Implementation Plan"
- Plan includes steps and files to modify

→ Correct status: Ready to Implement
→ Action: Update board status
```

### Example 4: New Issue with Clear Requirements
```
Issue #14: "Log compilation times"
- Current board status: Triage
- Has clear acceptance criteria
- Scope is well-defined
- No questions needed

→ Correct status: Ready to Plan
→ Action: Update board status, assess difficulty
```

## Guidelines

- Always check for linked PRs first - this is the strongest signal
- Be respectful and constructive in all comments
- Implementation plans should be detailed enough for another developer to follow
- Consider existing architecture when estimating difficulty
- Look at related code before categorizing
- Check if issues are duplicates or related to existing work
- If unsure about difficulty, err on the side of marking as harder
- Prioritize getting the board status correct over other actions

## Detecting Linked PRs

PRs can reference issues in several ways. Search for all of these:
- `Fixes #N`
- `Closes #N`
- `Resolves #N`
- `Fix #N`
- `Close #N`
- `Resolve #N`

Also check PR titles and branch names for issue numbers.

```bash
# Search for PRs mentioning an issue
gh pr list --search "#<issue-number>" --state all --json number,title,state,isDraft,mergedAt
```

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
