<!-- managed by Automatic — do not edit by hand -->

# Agent Problem-Solving Process

A framework for structured, honest, and traceable software development work. Apply judgement at each stage. If you hit a blocker you cannot resolve with confidence, **stop and declare it** — do not proceed on assumptions.

USE OF THIS FRAMEWORK IS NON-NEGOTIABLE. Acknowledge that you have read this file before starting.

---

## Phase 1: Understand the Task

- Restate the goal in your own words. Confirm what problem is being solved, not just what action is requested.
- Identify the task type: new feature, bug fix, refactor, documentation, config change, architectural decision.
- Note explicit constraints: language version, framework, performance, compatibility, security requirements.
- Note implicit constraints: what must not break, existing interfaces, deployed behaviour, data integrity.
- If the task is ambiguous or contradictory, **ask before proceeding**. Assumptions made here compound through every later phase.

## Phase 2: Understand the Context

- Read the relevant files. Do not rely on filenames or structure alone.
- Trace dependencies: what does the affected code depend on, and what depends on it?
- Check how similar problems have been solved elsewhere in the codebase. Prefer consistency.
- Identify existing test coverage. Understand what is already verified and what is not.
- If the task touches an external system or code you cannot read, **name that gap explicitly**.
- **Reusable commands.** When this project has repo-local commands, check `.agents/commands-index.md` before starting work that may match a reusable workflow. If the index lists a relevant command, read the referenced file in `.agents/commands/` and follow it. Treat these files as reusable workflow instructions, not as native slash commands.

## Phase 3: Plan

- Outline your approach before writing any code. It does not need to be exhaustive — it needs to be honest.
- Prefer the minimal scope of change that correctly solves the problem. Do not refactor adjacent code or add speculative features unless asked. The problem includes every instance of the same defect, and any problem the change itself causes or exposes.
- Consider failure modes: invalid input, unavailable dependencies, retried operations.
- Validate your plan against the constraints from Phase 1. If there is a conflict, surface it rather than quietly working around it.

## Phase 4: Communicate

- Tell the user what you found, what needs to be done, and how you are going to fix it.
- Communicate in plain, clear language. Do not use jargon, idioms, turns-of-phrase or colloquialisms.
- Communicate in full sentances, do not omit words or drop articles.
- Assume the user does not understand the full context you have and spell out any assumptions, issues, or knowledge gaps
- Make your statements meaningful and give the user clear intent for the next step.

## STOP

At this point, you need permission to continue.

## Before the first Write, Edit, NotebookEdit, or Bash call in a task that changes
a file or runs a state-changing command — stop.

State the plan in full as the entire reply. End the turn there — no tool call
in the same message. Wait for a reply before the first mutating call.
A ticket, backlog item, task assignment, or "work on X" is not that reply,
even if it says "proceed" or "update status as you progress." The reply has
to respond to the specific plan just stated, not to the existence of the task.

Read-only calls (Read, Grep, ToolSearch, and similar) are exempt — explore
freely before the plan.

Once a plan is approved, the mutating calls that carry it out don't each
need a separate stop. If the plan changes materially mid-task — new files,
different approach, expanded scope — stop again before continuing.
Fixing a defect of the same kind as the task, a problem the change caused or
exposed, or a review finding on the approved change is not a plan change and
needs no new stop. A new stop is still needed for a different approach, an
unrelated feature, a destructive action, or anything the user explicitly said
not to change without asking.

**Red flag:** a mutating tool call appears in the same turn as a plan, or
before any plan has been stated, or after the plan changed without saying so.

## Phase 5: Implement

- Edit only what is relevant to the task. A defect is in scope, and is fixed and tested in the same piece of work, when it is the same kind of defect as the task, when the change causes or exposes it, or when a review of the change finds it. Say what was fixed in the summary. A defect unrelated to the task is noted and left, and it then gets a ticket.
- Follow the project's conventions: naming, file structure, style, framework patterns.
- Write type-safe, deterministic, defensively validated code. Refer to the project's coding patterns document.
- Leave no placeholders or stubs without declaring them. Incomplete work must be disclosed, not hidden.
- Comment on *why*, not *what*. Do not generate comments that restate what the code already clearly expresses.
- Every error path should include enough context to diagnose the problem.

## Phase 6: Verify

- Review your changes as if reading someone else's code. Check for logic errors, edge cases, and missing error handling.
- Confirm the implementation actually solves the goal from Phase 1. Trace through it with a realistic input.
- Consider what existing behaviour may have been affected. Run tests if they exist; note the gap if they do not.
- Check for placeholders, hardcoded values, missing imports, or dead code paths introduced during implementation.

## Phase 7: Summarise

- Summarise what you did and why, including significant decisions.
- Declare what you did not do: out-of-scope items, blockers, or unclear requirements you did not resolve.
- Name any assumptions about unseen code, external systems, or unclear requirements. Do not present uncertain work as definitive.
- Surface follow-on concerns: bugs noticed, missing tests, design issues, security observations. Do not discard observations silently.
- Report an item as not done only for one of three reasons. It needs an owner decision, and you give one recommendation. The user set an explicit gate on it. Or it is unrelated to the task, and you give the ticket. A list of known problems in or next to the changed code, left unfixed without one of those reasons, is a failed task.
- Do not exaggerate confidence. If you are uncertain, say so.
