---
description: Git workflow rules — worktrees, branches, and PRs for all development
globs: "**/*"
---

# Git Workflow

## The Rule

**Never write changes on the main git worktree.** All development — including Claude-assisted changes — happens in a dedicated worktree on a feature branch, then lands via PR.

## Step-by-step

1. **Create a worktree + branch** before touching any code:
   ```sh
   git worktree add ../hvac-<short-slug> <branch-name>
   ```
   Pick a slug that describes the task (e.g. `hvac-fix-audio`, `hvac-add-qsv`).

2. **Work entirely inside the worktree** at `../hvac-<short-slug>/`.

3. **Commit** there as normal; push the branch:
   ```sh
   git -C ../hvac-<short-slug> push -u origin <branch-name>
   ```

4. **Open a PR** with `gh pr create`.

5. **Clean up** after the PR merges:
   ```sh
   git worktree remove ../hvac-<short-slug>
   git branch -d <branch-name>
   ```

## For Claude

When the user asks for any implementation work:

1. Create the worktree + branch first (step 1 above).
2. Make all edits inside that worktree.
3. Commit and push.
4. Open a PR and return the URL.

Do not stage or commit anything in the primary checkout (`/Users/jackdanger/www/hvac`).
