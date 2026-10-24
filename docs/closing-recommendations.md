## Generic closing procedure

For a repo with no campaign skill of its own. The aim is twofold: find issues that can be
closed now, and find issues that are cheaper to fix together, because closely related issues
fixed in one session pay for one gate pass instead of one each.

1. **Check each issue's claims first.** An issue is a hypothesis. Read it and its comments
   (`gh issue view <n> --repo <owner/repo> --comments`), then check what it says against the
   code: does the cited file, function or behaviour still exist? Was it changed since the
   issue was opened (`git log --since <opened> -- <path>`, `git log --grep '#<n>'`)?
2. **Sort each issue into one state.**
   - *Likely fixed*: a commit on the default branch addresses it. Name the commit; say what
     it changed and why that answers the issue. A commit that only mentions the number is not
     enough.
   - *Duplicate*: another issue in the project reports the same thing. Name the other one,
     preferring the older or the better-described.
   - *Obsolete*: overtaken by events (the code it is about was removed or replaced, the need
     went away). Say what overtook it.
   - *Bounded fix*: still open, and the fix is clear and contained.
   - *Needs design*: still open, and fixing it needs a decision first.
   - *Blocked*: waits on another issue or on something outside the repo.
   - *Keep open*: none of the above; leave it.
3. **Batch the bounded fixes by surface.** Two or more bounded fixes in one repo that touch
   the same code (the same module, file or subsystem) form a batch: one session fixes them
   together, with one gate after all of them. Name the shared surface. Do not batch across
   repos, and do not put an issue in two batches. A batch of unrelated issues saves nothing.
4. **Record every issue.** Likely fixed, duplicate and obsolete become suggestions; a batch
   becomes one `batch` suggestion; needs design, blocked and keep open go in `left_out` with
   their reason. Every issue in the list ends in exactly one place.

Confidence is your honest estimate that the owner, checking your reason, agrees. Prefer
fewer, well-founded recommendations over many guesses.
