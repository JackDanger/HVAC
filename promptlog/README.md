# Prompt log

Captured prompts from agent-assisted work sessions on this project,
published to <https://jackdanger.com/promptlog/> via the
`publish-promptlog` job in [`.github/workflows/ci.yml`](../.github/workflows/ci.yml).

## Why

For transparency about how this codebase is being built — what the
operator asked for, in their own words, in order — and as a reference
for the kind of prompts that produce the kind of changes you see in the
git log.

## Format

One Markdown file per session, named `YYYY-MM-DD-<short-handle>.md`.
Frontmatter records the date, participants, and a one-paragraph summary.
The body is a numbered list of the human's prompts in chronological
order, each as a verbatim blockquote with one line of context.

Each file is self-contained: it doesn't need the conversation transcript
to make sense to a reader of the repo.

## Publishing

The `publish-promptlog` CI job picks up any session file that's added or
modified in a push to `main` and POSTs it to
<https://jackdanger.com/promptlog/> as `text/markdown`. Authentication
is via the `PROMPTLOG_TOKEN` repository secret (`Authorization: Bearer
<token>`).

If the secret isn't configured, the job logs the upload it *would* have
made and exits zero — non-publishing forks of the repo aren't punished.
