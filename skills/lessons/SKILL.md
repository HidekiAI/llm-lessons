---
name: lessons
description: 'Search the lessons database before debugging or starting a task, and record a lesson when a problem is solved. Use at the start of any debugging, error, build or CI failure, "why is this failing" or "have we seen this before" task; before repeating an approach that failed once; and whenever a problem is resolved, a root cause is found, or an approach is ruled out. Also use when asked to "add a lesson", "record this", "search lessons", "check the db", or after any post-mortem. Wraps the `lessons` CLI (and the same tools if the lessons MCP server is registered).'
---

# Lessons

`lessons` is the owner's post-mortem database: problems, root causes, fixes and the attempts that did NOT work,
kept in SQLite files per domain (`work`, `personal`, `shared`) under `$LESSONS_HOME` (default `~/.lessons/`).
Use the installed binary (or the `search_lessons` and `record_lesson` MCP tools). Never open the `.db` with `sqlite3`.

## Before a task: search

1. Search with the exact error text or two or three keywords: `lessons search "<text>"`.
2. Choose the domains by where you are: at work `--in work,shared`, at home `--in personal,shared`. Without `--in`
   the configured default applies. A machine without a domain's file skips it.
3. Read the hits as leads, not as truth: check the date and context of the lesson against the present code.
4. If a lesson helped, `lessons hit <id>`. If it proved wrong, `lessons retire <id> --by <new-id>` after adding the
   corrected one.

## After a problem is solved: record

Record in the same session, while the exact error text is still in front of you.

```sh
lessons add --to <shared|personal|work> --title "<one line>" --problem "<what went wrong, with the exact error>" \
    --root-cause "<why>" --resolution "<what worked>" --failed "<what did NOT work>" --context "<repo or place>" \
    --tags "<space separated>"
```

- Pick the file by scope: `shared` generic, `personal` home, `work` employer (it may hold proprietary data and must
  never reach a public repo). `--to` is required: the domain is never guessed.
- Search first and update rather than duplicate: `lessons update <id> --resolution "..."` takes the same field flags (not `--to`).
- Keep the failed attempts: they are what saves the next reader time.
- Never put secrets, tokens or customer data in a lesson.

## Not for this skill

- Syncing between machines (`lessons sync`) and resolving sync conflicts are the owner's decisions, not a step to run
  unasked.
- If `lessons` is not on the PATH, say so in one line and keep the lesson in docs or memory to import later.
