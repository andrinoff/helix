# Commands

- [Typable commands](#typable-commands)
- [Static commands](#static-commands)

## Typable commands

Typable commands are used from command mode and may take arguments. Command mode can be activated by pressing `:`. The built-in typable commands are:

{{#include ./generated/typable-cmd.md}}

## Static Commands

Static commands take no arguments and can be bound to keys. Static commands can also be executed from the command picker (`<space>?`). The built-in static commands are:

{{#include ./generated/static-cmd.md}}

## Pull request review

Helix can review GitHub pull requests with the help of the [GitHub CLI](https://cli.github.com) (`gh`), which must be installed and authenticated (`gh auth login`). All `:pr*` commands below require it.

Typing `:pr` lists the open pull requests of the current repository; selecting one checks out its branch (all open documents are reloaded from disk) and opens a picker of the changed files. Diff review happens **inside the actual files**: each file opened during the review is diffed against the PR's base branch, so the added/removed lines are highlighted in place and all of Helix's features work on them — LSP go-to-definition and references, `]g`/`[g` hunk navigation, and `:reset-diff-change`. This works for any file opened while a review is active, not just ones picked from the list. A read-only overview of the full unified diff, including inline review comment blocks attributed to their GitHub usernames, is available with `:pr-diff`.

- `:pr` — list open pull requests and start reviewing the selected one
- `:pr <number>` — review a specific pull request directly
- `:pr-files` — pick a file changed by the pull request and open it in the editor
- `:pr-diff` — open the unified diff overview (with inline review comments) in a read-only buffer
- `:pr-comments` — browse all review comments; selecting one jumps to the commented file and line
- `:pr-comment [body...]` — record a pending review comment on the line under the cursor; without arguments a prompt asks for the body
- `:review approve|changes|comment [body...]` — publish the review with its pending comments

Comments are written locally first: they show up immediately (in the file and in the `:pr-diff` overview) and are only sent to GitHub when a review is published with `:review`, which submits all of them at once together with the verdict. `:review comment` publishes comments without a verdict, `:review approve` approves the pull request and `:review changes` requests changes.

Deleted lines are shown inline in the files as read-only rows, and comment blocks are colored by author (yellow by default, via the `review.comment.author` theme key) and body text (`review.comment.body`). Nothing is written into the file, so language servers are unaffected.

Comments can only be attached to lines that are part of the diff (like on GitHub); comments on lines of a previous diff revision are shown under a trailing outdated comments section.

If the remote is fetched over SSH and the key needs a passphrase, helix shows an in-editor prompt with the `ssh` question instead of hanging; the typed passphrase is masked, used for that checkout only and never stored.
