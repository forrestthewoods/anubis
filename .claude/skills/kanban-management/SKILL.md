---
name: kanban-management
description: Manages the Anubis Issue Tracker GitHub project board. Use when you need to organize issues by difficulty/status, move issues through workflow stages, or generate board status reports.
---

# Kanban Project Management

## Project Board

The **Anubis Issue Tracker** project board is already created and configured:
- **Project URL:** https://github.com/users/forrestthewoods/projects/8
- **Project Number:** 8
- **Owner:** forrestthewoods

**Important:** Do NOT create a new project board. Always use project #8.

## Board Status Workflow

The project board uses these seven status columns:

| Status | Description | Color |
|--------|-------------|-------|
| **Triage** | New issues that haven't been reviewed yet | Gray |
| **Waiting for Comment** | Issues waiting for a response/clarification | Blue |
| **Ready to Plan** | Issues with all needed information, ready for planning | Green |
| **Ready to Implement** | Issues with a plan ready to be worked on | Yellow |
| **In-progress** | Issues under active development | Orange |
| **Ready to Review** | Work done, ready for review and merge | Red |
| **Done** | Closed and completed | Pink |

## Purpose

This skill helps manage the existing project board by:
1. Adding new issues to the board
2. Moving issues through workflow stages
3. Organizing issues by difficulty labels
4. Generating board status reports
5. Maintaining board hygiene

## Instructions

### Step 1: View Current Board State

```bash
# List all items on the project board
gh project item-list 8 --owner forrestthewoods --format json

# View project details
gh project view 8 --owner forrestthewoods --format json
```

### Step 2: Add New Issues to Board

When new issues are created, add them to the board:

```bash
# Add a single issue
gh project item-add 8 --owner forrestthewoods --url https://github.com/forrestthewoods/anubis/issues/<number>

# Add all open issues not yet on board
gh issue list --state open --json url -q '.[].url' | while read url; do
  gh project item-add 8 --owner forrestthewoods --url "$url" 2>/dev/null
done
```

### Step 3: Move Issues Through Stages

Update issue status based on progress:

```bash
# Get project and field IDs
gh project view 8 --owner forrestthewoods --format json

# Get item ID for specific issue
gh project item-list 8 --owner forrestthewoods --format json | jq '.items[] | select(.content.number == <issue-number>)'

# Update status
gh project item-edit --project-id <project-id> --id <item-id> --field-id <status-field-id> --single-select-option-id <option-id>
```

**Status Transition Rules:**

| From | To | Trigger |
|------|-----|---------|
| Triage | Waiting for Comment | Clarifying questions posted |
| Triage | Ready to Plan | Issue has sufficient detail |
| Waiting for Comment | Ready to Plan | User responds with needed info |
| Ready to Plan | Ready to Implement | Implementation plan written |
| Ready to Implement | In-progress | Developer starts work |
| In-progress | Ready to Review | PR submitted |
| Ready to Review | Done | PR merged and issue closed |

### Step 4: Manage Difficulty Labels

Ensure issues have appropriate difficulty labels:

```bash
# Create difficulty labels (if not exist)
gh label create "difficulty: easy" --color "0E8A16" --description "Simple change, good first issue" --force
gh label create "difficulty: medium" --color "FBCA04" --description "Moderate complexity" --force
gh label create "difficulty: hard" --color "D93F0B" --description "Complex, requires significant effort" --force

# Add label to issue
gh issue edit <number> --add-label "difficulty: easy|medium|hard"
```

### Step 5: Generate Board Status Report

```markdown
## Anubis Issue Tracker - Board Status

**Project:** https://github.com/users/forrestthewoods/projects/8

### Summary
- **Total Issues:** 22
- **Triage:** 0
- **Waiting for Comment:** 5
- **Ready to Plan:** 10
- **Ready to Implement:** 4
- **In-progress:** 1
- **Ready to Review:** 0
- **Done:** 2

### By Difficulty

| Difficulty | Triage | Waiting | Ready to Plan | Ready to Impl | In-progress | Review | Done |
|------------|--------|---------|---------------|---------------|-------------|--------|------|
| Easy       | 0      | 1       | 3             | 2             | 0           | 0      | 1    |
| Medium     | 0      | 2       | 4             | 1             | 1           | 0      | 1    |
| Hard       | 0      | 2       | 3             | 1             | 0           | 0      | 0    |

### Ready to Implement (Prioritized)
These issues have plans and are ready for work:

**Easy (Quick Wins):**
1. #10 - Fix typo in README
2. #11 - Add --help examples

**Medium:**
1. #18 - Add user preferences

**Hard:**
1. #22 - Refactor build system

### Waiting for Comment
These need responses before proceeding:
- #4 - Asked about reproduction steps (3 days ago)
- #9 - Asked about expected behavior (1 day ago)

### In-progress
Currently being worked on:
- #25 - Parallel builds (@developer, started 2 days ago)

### Stale Items
Items that haven't been updated in 14+ days:
- #8 - In "Ready to Plan" since Dec 1
- #12 - In "Waiting for Comment" since Nov 28
```

### Step 6: Board Hygiene Tasks

**Weekly cleanup checklist:**

1. **Check for orphaned issues:**
   ```bash
   # Find open issues not on board
   gh issue list --state open --json number,title | jq -r '.[] | "#\(.number) - \(.title)"'
   ```

2. **Archive completed items older than 30 days:**
   - Items in "Done" status can be archived to keep board clean

3. **Flag stale in-progress items:**
   - Issues in "In-progress" for more than 14 days may need attention

4. **Verify labels match board status:**
   - Issues with "difficulty: easy" should generally not stay in "Waiting for Comment" long

5. **Check for issues needing triage:**
   - New issues should be moved from "Triage" within 2 business days

## Automation Notes

The project board has 8 automated workflows that handle:
- Moving items when issues are closed
- Moving items when issues are reopened
- Updating status when PRs are linked

These automations run automatically. Manual status updates are needed for:
- Triage decisions
- Adding implementation plans
- Moving from "Ready to Plan" to "Ready to Implement"

## Guidelines

- Keep the board focused on actionable items
- Update status when actual progress changes
- Don't let items sit in "In-progress" indefinitely
- Review stale items weekly
- Use consistent labeling between issues and board
- Prioritize items in "Triage" status quickly
- Ensure all open issues are on the board

## Quick Reference Commands

```bash
# View board
gh project view 8 --owner forrestthewoods --web

# List all items
gh project item-list 8 --owner forrestthewoods

# Add issue to board
gh project item-add 8 --owner forrestthewoods --url <issue-url>

# List open issues
gh issue list --state open

# View specific issue
gh issue view <number>

# Edit issue labels
gh issue edit <number> --add-label "difficulty: easy"
```
