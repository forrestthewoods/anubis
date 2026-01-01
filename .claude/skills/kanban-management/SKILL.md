---
name: kanban-management
description: Creates and manages GitHub project boards for issue tracking. Use when you need to set up a Kanban board, organize issues by difficulty/status, or move issues through workflow stages.
---

# Kanban Project Management

## Purpose

This skill helps manage GitHub project boards by:
1. Creating project boards with appropriate columns
2. Adding issues to boards organized by difficulty
3. Moving issues through workflow stages
4. Maintaining board hygiene and organization

## Instructions

### Step 1: Check Existing Projects

```bash
# List existing projects
gh project list --owner <owner> --format json

# View project details
gh project view <project-number> --owner <owner> --format json
```

### Step 2: Create Project Board (if needed)

```bash
# Create a new project
gh project create --owner <owner> --title "Anubis Issue Tracker"
```

### Step 3: Set Up Custom Fields

Create fields for tracking:

```bash
# Add Status field (single select)
gh project field-create <project-number> --owner <owner> \
  --name "Status" \
  --data-type "SINGLE_SELECT" \
  --single-select-options "Backlog,Ready,In Progress,In Review,Done"

# Add Difficulty field (single select)
gh project field-create <project-number> --owner <owner> \
  --name "Difficulty" \
  --data-type "SINGLE_SELECT" \
  --single-select-options "Easy,Medium,Hard"

# Add Sprint/Iteration field (optional)
gh project field-create <project-number> --owner <owner> \
  --name "Sprint" \
  --data-type "ITERATION"
```

### Step 4: Add Issues to Project

```bash
# Add a single issue
gh project item-add <project-number> --owner <owner> --url <issue-url>

# Add all open issues (batch)
gh issue list --state open --json url --jq '.[].url' | while read url; do
  gh project item-add <project-number> --owner <owner> --url "$url"
done
```

### Step 5: Organize Issues by Difficulty

Set difficulty based on labels or analysis:

```bash
# Get item ID for an issue
gh project item-list <project-number> --owner <owner> --format json

# Update item field
gh project item-edit --project-id <project-id> --id <item-id> \
  --field-id <difficulty-field-id> --single-select-option-id <option-id>
```

### Step 6: Manage Workflow Stages

**Stage Definitions:**

| Stage | Description | Entry Criteria |
|-------|-------------|----------------|
| **Backlog** | Not yet ready | Needs clarification or planning |
| **Ready** | Ready to start | Has implementation plan, no blockers |
| **In Progress** | Being worked on | Developer assigned, work started |
| **In Review** | PR submitted | Awaiting code review |
| **Done** | Completed | Merged and closed |

**Moving Issues Through Stages:**

```bash
# Move to Ready (after implementation plan is written)
gh project item-edit --project-id <project-id> --id <item-id> \
  --field-id <status-field-id> --single-select-option-id <ready-option-id>

# Move to In Progress (when work starts)
gh project item-edit --project-id <project-id> --id <item-id> \
  --field-id <status-field-id> --single-select-option-id <in-progress-option-id>
```

### Step 7: Create Board Views

**By Status (Kanban view):**
- Group by: Status field
- Sort by: Difficulty (Easy first for quick wins)

**By Difficulty (Planning view):**
- Group by: Difficulty field
- Sort by: Status (Ready items first)

**By Assignee (Workload view):**
- Group by: Assignee
- Sort by: Status

### Step 8: Generate Board Status Report

```markdown
## Project Board Status

### Summary
- **Total Issues:** 25
- **Backlog:** 8
- **Ready:** 5
- **In Progress:** 4
- **In Review:** 3
- **Done:** 5

### By Difficulty

| Difficulty | Backlog | Ready | In Progress | In Review | Done |
|------------|---------|-------|-------------|-----------|------|
| Easy       | 2       | 3     | 1           | 1         | 3    |
| Medium     | 4       | 1     | 2           | 1         | 2    |
| Hard       | 2       | 1     | 1           | 1         | 0    |

### Ready for Work (Prioritized)
These issues are ready to start:

**Easy (Quick Wins):**
1. #10 - Fix typo in README
2. #11 - Add --help examples
3. #14 - Update error message

**Medium:**
1. #18 - Add user preferences

**Hard:**
1. #22 - Refactor build system

### Blocked Issues
These need attention before they can proceed:
- #25 - Waiting on #15 (in progress)
- #27 - Needs clarification (question posted)

### Stale Items
These haven't been updated in 14+ days:
- #8 - In Progress since Dec 1
- #12 - In Review since Nov 28
```

## Automation Rules

### Auto-move on PR creation
When a PR references an issue, move to "In Review":
```bash
# When PR is created referencing #N
gh project item-edit --project-id <project-id> --id <item-id> \
  --field-id <status-field-id> --single-select-option-id <review-option-id>
```

### Auto-move on PR merge
When PR is merged, move to "Done" and close issue:
```bash
# The issue should auto-close if PR uses "Fixes #N"
# Project item moves to Done automatically
```

### Weekly cleanup
Run periodically to maintain board hygiene:
1. Archive completed items older than 30 days
2. Flag stale in-progress items
3. Check for issues missing from board
4. Verify labels match board fields

## Guidelines

- Keep the board focused on actionable items
- Archive completed work regularly
- Use consistent labeling between issues and board
- Update status when actual progress changes
- Don't let items sit in "In Progress" indefinitely
- Review stale items weekly

## Labels to Sync with Board

Ensure these labels exist in the repository:

```bash
# Create difficulty labels
gh label create "difficulty: easy" --color "0E8A16" --description "Simple change, good first issue"
gh label create "difficulty: medium" --color "FBCA04" --description "Moderate complexity"
gh label create "difficulty: hard" --color "D93F0B" --description "Complex, requires significant effort"

# Create status labels (optional, board is primary)
gh label create "status: ready" --color "0052CC" --description "Ready for implementation"
gh label create "status: in-progress" --color "5319E7" --description "Work in progress"
gh label create "status: blocked" --color "B60205" --description "Blocked by dependency"
```

## Example Board Setup Workflow

```bash
# 1. Create project
PROJECT_URL=$(gh project create --owner forrestthewoods --title "Anubis Issue Tracker" --format json | jq -r '.url')

# 2. Get project number from URL
PROJECT_NUM=1  # Extract from URL

# 3. Add status field
gh project field-create $PROJECT_NUM --owner forrestthewoods \
  --name "Status" --data-type "SINGLE_SELECT" \
  --single-select-options "Backlog,Ready,In Progress,In Review,Done"

# 4. Add difficulty field
gh project field-create $PROJECT_NUM --owner forrestthewoods \
  --name "Difficulty" --data-type "SINGLE_SELECT" \
  --single-select-options "Easy,Medium,Hard"

# 5. Add all open issues
gh issue list --state open --json url -q '.[].url' | while read url; do
  gh project item-add $PROJECT_NUM --owner forrestthewoods --url "$url"
done

# 6. Report success
echo "Project board created and populated with $(gh issue list --state open | wc -l) issues"
```
