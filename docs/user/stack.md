# Stack Multiple PRs

The differences between jj-spr's commit-based workflow and GitHub's default branch-based workflow are most apparent when you have multiple reviews in flight at the same time.

This guide assumes you're already familiar with the workflow for [simple, non-stacked PRs](./simple.md).

## Two Approaches: Independent vs Dependent

When working with multiple PRs, you have two main approaches:

1. **Independent changes with `--cherry-pick`** (recommended for most cases)
   - Changes can be landed in any order
   - Simpler workflow, fewer manual steps
   - Best when changes don't strictly depend on each other
   - Example: Bug fix + new feature in same codebase area

2. **Dependent stacks** (for true dependencies)
   - Changes must be landed in order (parent → child)
   - More complex workflow with manual rebasing
   - Only use when second change literally won't work without first
   - Example: Add database table → Add API endpoints using that table

**Not sure which to use?** Start with `--cherry-pick`. You can always switch to dependent stacks if needed.

In Jujutsu, managing stacked changes is much simpler than in Git because Jujutsu maintains stable change IDs and automatically handles rebasing operations.

## Independent Changes with --cherry-pick (Recommended)

**For most users, this is the workflow you want.** It's simpler and more flexible than dependent stacks.

### Creating Independent Changes

1. Create your first change:
   ```shell
   jj new main@origin
   # Make changes...
   jj describe -m "Add authentication module"
   jj new  # Move to empty working copy, PR change at @-
   jj spr diff  # Create PR #123
   ```

2. Create your second change (also based on main):
   ```shell
   jj new main@origin
   # Make changes...
   jj describe -m "Fix login bug"
   jj new  # Move to empty working copy, PR change at @-
   jj spr diff --cherry-pick  # Create PR #124 as independent
   ```

The `--cherry-pick` flag tells jj-spr to create the PR as if the change were based directly on `main@origin`, even if locally you have both changes in a stack.

### Landing Independent Changes

You can land these in **any order:**

```shell
# Land either one first
jj spr land --cherry-pick -r <change-id>

# Then the other
jj spr land --cherry-pick -r <other-change-id>
```

**After landing, you still need to rebase:**
```shell
jj git fetch
jj rebase -r @ -d main@origin
```

**Benefits:**
- ✅ Land in any order
- ✅ Simpler workflow
- ✅ Less chance of breaking your stack
- ✅ Reviewers see clean, focused PRs

## Dependent Stacks (Advanced)

**Only use this if your changes truly depend on each other.** The workflow is more complex.

### Creating Dependent Changes

This is for when the second change literally won't work without the first.

1. Create your first change on top of `main`:
   ```shell
   jj new main@origin
   # Make changes...
   jj describe -m "Add authentication module"
   ```

2. Create your second change on top of the first:
   ```shell
   jj new
   # Make changes that depend on the authentication module...
   jj describe -m "Add user profile endpoints"
   ```

3. Create an empty working copy on top:
   ```shell
   jj new
   ```

   Now your stack looks like:
   - `@` = empty working copy
   - `@-` = "Add user profile endpoints" (second PR change)
   - `@--` = "Add authentication module" (first PR change)

4. Run `jj spr diff --all` to create PRs for all changes in your stack:
   ```shell
   jj spr diff --all
   ```

   This is equivalent to calling `jj spr diff` on each change in your stack from `@-` back to `main@origin`.

## Native GitHub Stacks

By default, a dependent PR targets a base branch that jj-spr creates for it
(`<prefix>main.<title>`), containing the files of the change below it. GitHub
can also group dependent PRs into a
[stack](https://docs.github.com/en/pull-requests/how-tos/stacked-pull-requests),
shown as one unit in its UI. That requires each PR to target the PR branch of
the change below it, which jj-spr does when native stacks are enabled:

```shell
jj config set --repo spr.nativeStacks true   # or pass --native-stack to jj spr diff
jj spr diff --all
```

With native stacks:

- Each PR targets its parent change's PR branch, and jj-spr creates no base
  branches of its own. Updates still add a commit to the PR branch (with your
  `-m` message), and each update commit merges in the parent PR's current head,
  so GitHub keeps showing only the change itself.
- After publishing, `jj spr diff` registers the chain as a GitHub stack, or
  appends new PRs to the existing one. GitHub does not allow changing the base
  of a stacked PR, so when you reorder, insert or remove changes, `jj spr diff
  --all` dissolves the stack first and rebuilds it. It refuses to do that if
  the stack contains PRs the run would not update; run it from the top of the
  stack instead.
- Without `--all`, a change stacks on its parent's existing PR, so the parent
  must have been submitted already.
- `jj spr land` only lands the bottom PR of a stack, and merges it through
  GitHub's stack-aware merge. GitHub then retargets the next PR to the main
  branch and rebases the rest of the stack. Afterwards, rebase your remaining
  changes onto the main branch and run `jj spr diff --all`.
- Existing PRs that use jj-spr base branches are moved over the next time you
  run `jj spr diff --all`; the old base branches can be removed with
  `jj spr cleanup`.
- If stacked pull requests are not enabled for the repository, the PRs still
  target each other; they just aren't registered as a GitHub stack.

Limitations: repositories that merge through a merge queue are not supported
yet, and a PR landed from a stack gets GitHub's default squash-commit message
rather than one built from the PR description.

## Understanding Your Stack

Use `jj log` to visualize your stack:
```shell
jj log -r 'main@origin..'
```

**Example output:**
```
@  qpvuntsm you@example.com 2024-01-15 12:00:00
│  (empty)
○  kmkuslkw you@example.com 2024-01-15 11:30:00
│  Add user profile endpoints
○  rlvkpnrw you@example.com 2024-01-15 11:00:00
│  Add authentication module
◆  main@origin
```

In this example:
- `@` = empty working copy
- `kmkuslkw` = second change (depends on first)
- `rlvkpnrw` = first change (base of stack)
- Change IDs are in the first column

This shows all your changes that are descendants of `main@origin`.

### Visual: Local Stack vs GitHub PRs

Here's what the above stack looks like locally vs on GitHub:

```
Local Jujutsu State:                 GitHub State:

@  qpvuntsm (empty)
│
○  kmkuslkw                     →    PR #124: "Add user profile endpoints"
│  Add user profile endpoints        base: jj-spr-rlvkpnrw (PR #123's branch)
│                                    branch: jj-spr-kmkuslkw
○  rlvkpnrw                     →    PR #123: "Add authentication module"
│  Add authentication module         base: main
│                                    branch: jj-spr-rlvkpnrw
◆  main@origin
```

**Key points:**
- Each change has a unique ID (`rlvkpnrw`, `kmkuslkw`)
- jj-spr creates GitHub branches automatically (`jj-spr-{change-id}`)
- Stacked PRs: PR #124 is based on PR #123's branch
- When PR #123 lands, PR #124 automatically updates to be based on `main`

## Updating Changes in the Stack

Suppose you need to update the first change (authentication module with ID `rlvkpnrw`) in response to review feedback.

**Step 1: Find the change ID**

First, identify which change you want to edit:
```shell
jj log -r 'main@origin..'
```

Output:
```
@  qpvuntsm (empty)
○  kmkuslkw Add user profile endpoints
○  rlvkpnrw Add authentication module  ← This is the one we want to update
◆  main@origin
```

**Method 1: Squash from working copy (Recommended)**

This is the recommended approach that aligns with the standard workflow:

1. Make your changes in your working copy (`@`):
   ```shell
   # You're already at @ (empty working copy)
   # Make your edits...
   ```

2. Squash the changes into the target change:
   ```shell
   jj squash --into rlvkpnrw  # Use the actual change ID
   ```

3. Update the PR:
   ```shell
   jj spr diff -r rlvkpnrw
   ```

**Method 2: Direct editing (jj edit)**

Alternatively, edit the change directly:

1. Edit the change directly:
   ```shell
   jj edit rlvkpnrw  # Use the actual change ID
   # Make your changes...
   ```

2. The changes are automatically absorbed. Jujutsu will automatically rebase descendant changes.

3. Update the PR for that specific change:
   ```shell
   jj spr diff -r @  # @ is now rlvkpnrw since we edited it
   ```

4. Return to your empty working copy:
   ```shell
   jj new qpvuntsm  # Creates new working copy on top of qpvuntsm
   ```

**Which method to use?**
- Use `jj squash` (Method 1) for consistency with the standard workflow
- Use `jj edit` (Method 2) if you want to work directly on the change

## Landing Stacked Changes

> 🚨 **CRITICAL WARNINGS FOR STACKED LANDING:**
>
> 1. **Landing order matters:** Always land changes in order (parent before child). Landing out of order **will** cause merge conflicts and break your stack.
>
> 2. **Manual rebasing required:** After **every** `jj spr land`, you must manually rebase both your working copy AND any remaining changes in the stack. This is error-prone and easy to forget.

### Landing Process (Parent Change)

Using our example stack where `rlvkpnrw` (auth module) is the parent and `kmkuslkw` (user profiles) is the child:

```
Before landing:
○  kmkuslkw  Add user profile endpoints (PR #124)
○  rlvkpnrw  Add authentication module (PR #123)  ← Land this first
◆  main@origin
```

**Step-by-step:**

1. **Land the parent change:**
   ```shell
   jj spr land -r rlvkpnrw  # Use the actual change ID
   ```

2. **REQUIRED - Fetch and rebase working copy:**
   ```shell
   jj git fetch
   jj rebase -r @ -d main@origin
   ```

3. **REQUIRED - Rebase child changes onto new main:**
   ```shell
   # Check what needs rebasing
   jj log -r 'main@origin..'

   # Rebase the child change
   jj rebase -s kmkuslkw -d main@origin  # kmkuslkw is now based on main
   ```

4. **REQUIRED - Update remaining PRs:**
   ```shell
   jj spr diff --all  # Updates PR #124 to be based on main instead of PR #123
   ```

**After landing:**
```
○  kmkuslkw  Add user profile endpoints (PR #124, now based on main)
◆  main@origin (now includes rlvkpnrw)
```

**This is 4 commands just to land ONE change.** If you skip any step, your stack will be broken.

### Best Practices

- ✅ **Always land in order:** Parent → Child → Grandchild
- ✅ **Double-check change IDs** before landing (use `jj log`)
- ✅ **If changes aren't truly dependent, use `--cherry-pick` instead** (see above)
- ❌ **Never land out of order** unless you're prepared to manually fix merge conflicts

## Rebasing the Whole Stack

One of the major advantages of Jujutsu is that rebasing your entire stack onto new upstream changes is trivial:

1. Fetch the latest changes:
   ```shell
   jj git fetch
   ```

2. Rebase your stack:
   ```shell
   jj rebase -s <root-change-id> -d main@origin
   ```

   Where `<root-change-id>` is the first change in your stack.

3. Update all PRs:
   ```shell
   jj spr diff --all
   ```

## Working with Revsets

Jujutsu's revset language makes it easy to work with stacks:

```shell
# Show all your changes not yet in main
jj log -r 'mine() & ~main@origin'

# Create PRs for all your ready changes
jj spr diff --all -r 'ready() & ~main@origin'

# Show changes that have PRs
jj log -r 'description(regex:"#[0-9]+")'
```

## Tips for Stack Management

1. **Keep changes focused**: Each change should represent one logical unit of work.

2. **Use descriptive commit messages**: This helps when navigating your stack.

3. **Leverage change IDs**: Unlike Git commits, Jujutsu change IDs remain stable through rebases.

4. **Use `jj split` when needed**: If a change gets too large, split it:
   ```shell
   jj split -r <change-id>
   ```

5. **Monitor your stack**: Regularly run `jj log` to understand your stack's structure.

The Jujutsu + jj-spr workflow makes stacked PRs feel natural and eliminates much of the complexity found in traditional Git-based stacking workflows.
