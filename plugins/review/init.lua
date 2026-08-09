-- Code review in a side thread. The reviewer gets its own session, its own
-- rubric, and read-only tools, so the parent conversation pays for the
-- findings instead of the whole investigation.
--
-- The rubric and the target prompts are adapted from OpenAI Codex
-- (codex-rs/prompts/templates/review, Apache-2.0).

local ListPicker = require("maki.list_picker")
local ToolView = require("maki.tool_view")
local output_limits = require("maki.output_limits")
local helpers = require("review_helpers")

local TOOL_NAME = "review"
local REVIEWER_NAME = "reviewer"
local REVIEWER_AUDIENCE = "research_sub"
local REVIEWER_EXCEPT = { "task", "todo_write", "websearch", "webfetch", "view_image", "question" }
local FINDINGS_TOOL = "review_findings"
local FINDINGS_ACK = "Findings recorded."
local FINDINGS_DESCRIPTION = "Report the review result. Call it exactly once, when the review is complete."
local FINDINGS_MISSING = "reviewer finished without calling " .. FINDINGS_TOOL
local FINDINGS_NUDGE = "You did not call " .. FINDINGS_TOOL .. ". Call it now with your verdict and every finding."
local FINDINGS_INVALID = "Input does not match the required schema. Fix these errors and call "
  .. FINDINGS_TOOL
  .. " again:\n"
local MAX_NUDGES = 2
local MAX_SCHEMA_ERRORS = 3
local REVIEWER_ERROR = "reviewer error: "
local EMPTY_INSTRUCTIONS = "instructions must not be empty"

local PICKER_TITLE = " Review "
local PICKER_FOOTER = { { "Enter", "select" }, { "Esc", "close" } }
local BRANCH_TITLE = " Base branch "
local COMMIT_TITLE = " Commit "
local TARGETS = {
  { label = "Uncommitted changes", detail = "staged, unstaged, and untracked", kind = "uncommitted" },
  { label = "Against a base branch", detail = "diff from the merge base", kind = "base" },
  { label = "A single commit", detail = "pick from recent commits", kind = "commit" },
}
local NO_BRANCHES = "No other branches found"
local NO_COMMITS = "No commits found"
local GIT_TIMEOUT_MS = 5000
local BRANCH_CMD = "git for-each-ref --format='%(refname:short)' --sort=-committerdate --count=30 refs/heads"
local COMMIT_CMD = "git log --max-count=30 --pretty=format:%h%x09%s"
local MERGE_BASE_CMD = "git merge-base HEAD %s"
local SUBMIT_FAILED = "Failed to start the review: "
local REQUEST = "Run the %s tool now, with these instructions:\n\n%s\n\n"
  .. "Do not review the change yourself and do not repeat the findings back to me. "
  .. "Answer with one short line saying whether anything came up."

local DEFAULT_OUTPUT_LINES = 20
local BODY_INDENT_COLS = 4
local MIN_MD_WIDTH = 20

local RUBRIC = [==[
# Review guidelines

You are acting as a reviewer for a proposed code change made by another engineer.

Below are the default guidelines for deciding whether the author would appreciate an issue being flagged. They are
not the final word. More specific guidance, whether in the user's request, a project instruction file, or the code
itself, overrides them.

Flag a finding when:

1. It meaningfully impacts the accuracy, performance, security, or maintainability of the code.
2. It is discrete and actionable, not a general complaint about the codebase.
3. Fixing it does not demand more rigor than the rest of the codebase shows.
4. It was introduced by the change under review. Pre-existing bugs are out of scope.
5. The author would likely fix it once they knew about it.
6. It does not rest on unstated assumptions about the codebase or the author's intent.
7. Speculation is not enough. Name the other code that is provably affected.
8. It is clearly not an intentional change by the author.

Every finding carries a comment:

1. Be clear about why the issue is a bug.
2. Communicate the severity honestly. Never inflate it.
3. Keep the body to one paragraph, without line breaks unless a code fragment needs them.
4. Use no code chunk longer than 3 lines, and wrap code in backticks or a fenced block.
5. State the scenarios, environments, or inputs the bug needs, right away.
6. Keep the tone matter-of-fact. Not accusatory, not flattering.
7. Write so the author grasps the point without close reading.
8. Skip praise and anything else that does not help the author.

## How many findings to return

Report every finding the author would fix if they knew about it. Do not stop at the first one. If nothing clears
that bar, report no findings at all.

## Guidelines

- Ignore trivial style unless it obscures meaning or breaks a documented standard.
- One finding per distinct issue.
- Keep each line range as short as the issue allows, under 5 to 10 lines, and overlapping the change.
- Follow the project instruction files that apply to the changed files, the more specific file winning. When a rule
  drives a finding, cite the file and its lines. Never invent a citation.
- Report problems. Do not write the fix.

## Priorities

Tag every finding with a priority:

- 0: drop everything. Blocks release or major usage, with no assumptions about inputs.
- 1: urgent, address it next cycle.
- 2: normal, fix it eventually.
- 3: low, nice to have.

Investigate with the tools you have, starting with git to see the change. Then call review_findings exactly once.
The verdict is "correct" when existing code and tests keep working and the change is free of blocking issues, and
"incorrect" otherwise. Style, formatting, typos, and documentation nits never make a change incorrect.
]==]

local description = "Review a code change in a separate reviewer thread. It reads the diff on its own and returns "
  .. "prioritized findings, so use it instead of reviewing large changes inline."

local schema = {
  type = "object",
  required = { "instructions" },
  additionalProperties = false,
  properties = {
    instructions = {
      type = "string",
      description = 'What to review, e.g. "Review the code changes introduced by commit abc123 with `git show`." '
        .. "Name the diff, commit, or range so the reviewer can find the change itself.",
    },
    hint = {
      type = "string",
      description = 'Short label for the run, e.g. "current changes". Defaults to the instructions.',
    },
  },
}

local findings_schema = {
  type = "object",
  required = { "verdict", "explanation", "findings" },
  additionalProperties = false,
  properties = {
    verdict = {
      type = "string",
      enum = { "correct", "incorrect" },
      description = "Whether the change is free of blocking issues.",
    },
    explanation = { type = "string", description = "One to three sentences justifying the verdict." },
    findings = {
      type = "array",
      description = "Every issue worth flagging, or an empty array.",
      items = {
        type = "object",
        required = { "title", "body", "priority", "file", "start_line", "end_line" },
        additionalProperties = false,
        properties = {
          title = { type = "string", description = "Imperative, 80 characters or fewer." },
          body = { type = "string", description = "One paragraph of Markdown explaining why this is a problem." },
          priority = { type = "integer", description = "0 blocking, 1 urgent, 2 normal, 3 low." },
          file = { type = "string", description = "Path to the file holding the issue." },
          start_line = { type = "integer", description = "First line of the issue." },
          end_line = { type = "integer", description = "Last line of the issue." },
        },
      },
    },
  },
}

local examples = {
  {
    instructions = "Review the current code changes (staged, unstaged, and untracked files) and provide prioritized "
      .. "findings.",
    hint = "current changes",
  },
}

-- Compiled once: a schema that cannot compile is a plugin bug, and the
-- reviewer would otherwise discover it mid-run.
local validator, validator_err = maki.json.schema_validator(findings_schema)
if validator_err then
  error(FINDINGS_TOOL .. " schema: " .. validator_err, 0)
end

local function bounded_errors(errors)
  local out = {}
  for i = 1, math.min(#errors, MAX_SCHEMA_ERRORS) do
    out[i] = errors[i]
  end
  return table.concat(out, "\n")
end

local function handler(input, ctx)
  local instructions = (input.instructions or ""):match("^%s*(.-)%s*$")
  if instructions == "" then
    return { llm_output = EMPTY_INSTRUCTIONS, is_error = true }
  end

  local tool_defs, tools_err = maki.agent.tools(ctx, {
    audience = REVIEWER_AUDIENCE,
    except = REVIEWER_EXCEPT,
  })
  if tools_err then
    return { llm_output = tools_err, is_error = true }
  end

  local captured, last_errors
  local sess, sess_err = maki.agent.session(ctx, {
    system = RUBRIC,
    tools = tool_defs,
    audience = REVIEWER_AUDIENCE,
    name = REVIEWER_NAME,
    mcp = false,
    local_tools = {
      [FINDINGS_TOOL] = {
        description = FINDINGS_DESCRIPTION,
        input_schema = findings_schema,
        handler = function(value)
          local errors = validator:validate(value)
          if errors then
            last_errors = bounded_errors(errors)
            return nil, FINDINGS_INVALID .. last_errors
          end
          captured = value
          return FINDINGS_ACK
        end,
      },
    },
  })
  if sess_err then
    return { llm_output = sess_err, is_error = true }
  end

  local _, err = sess:prompt(instructions)
  local nudges = 0
  while not err and not captured and nudges < MAX_NUDGES do
    nudges = nudges + 1
    _, err = sess:prompt(FINDINGS_NUDGE)
  end
  sess:close()

  if err then
    return { llm_output = REVIEWER_ERROR .. err, is_error = true }
  end
  if not captured then
    local message = last_errors and (FINDINGS_MISSING .. ":\n" .. last_errors) or FINDINGS_MISSING
    return { llm_output = message, is_error = true }
  end
  return { llm_output = helpers.format_result(captured, input.hint or instructions), format = "markdown" }
end

local function restore(_input, output, is_error, ctx)
  local tol = ctx:tool_output_lines()
  return ToolView.restore_markdown(output, is_error, {
    max_lines = (tol and tol.other) or DEFAULT_OUTPUT_LINES,
    keep = "head",
    max_line_bytes = output_limits.DEFAULT_MAX_LINE_BYTES,
    width = math.max(maki.ui.terminal_size().cols - BODY_INDENT_COLS, MIN_MD_WIDTH),
  })
end

maki.api.register_tool({
  name = TOOL_NAME,
  description = description,
  kind = "execute",
  audiences = { "main" },
  examples = examples,
  schema = schema,
  handler = handler,
  header = function(input)
    return input.hint or input.instructions
  end,
  restore = restore,
})

local function git(command)
  local result = maki.fn.jobwait(maki.fn.jobstart(command), GIT_TIMEOUT_MS)
  if not result or result.exit_code ~= 0 then
    return nil
  end
  return result.stdout
end

local function pick(items, title)
  local result = ListPicker.open(items, { title = title, footer = PICKER_FOOTER })
  return result.type == "choice" and result.index or nil
end

local function pick_branch()
  local branches = helpers.parse_branches(git(BRANCH_CMD))
  if not branches or #branches == 0 then
    maki.ui.flash(NO_BRANCHES)
    return nil
  end
  local index = pick(branches, BRANCH_TITLE)
  if not index then
    return nil
  end
  local branch = branches[index]
  local merge_base = git(MERGE_BASE_CMD:format(branch))
  return { kind = "base", branch = branch, merge_base = merge_base and merge_base:match("^%x+") or nil }
end

local function pick_commit()
  local commits = helpers.parse_commits(git(COMMIT_CMD))
  if not commits or #commits == 0 then
    maki.ui.flash(NO_COMMITS)
    return nil
  end
  local items = {}
  for i, commit in ipairs(commits) do
    items[i] = { label = commit.title, detail = commit.sha }
  end
  local index = pick(items, COMMIT_TITLE)
  if not index then
    return nil
  end
  return { kind = "commit", sha = commits[index].sha, title = commits[index].title }
end

local function submit(target)
  local prompt, hint = helpers.resolve_target(target)
  if prompt == "" then
    return
  end
  local _, err = maki.session.prompt(REQUEST:format(TOOL_NAME, prompt))
  if err then
    maki.ui.flash(SUBMIT_FAILED .. tostring(err))
    return
  end
  maki.ui.flash("Reviewing " .. hint)
end

local function open(opts)
  local instructions = opts and opts.args or ""
  if instructions:match("%S") then
    submit({ kind = "custom", instructions = instructions })
    return
  end
  local index = pick(TARGETS, PICKER_TITLE)
  if not index then
    return
  end
  local kind = TARGETS[index].kind
  if kind == "uncommitted" then
    submit({ kind = kind })
  elseif kind == "base" then
    local target = pick_branch()
    if target then
      submit(target)
    end
  else
    local target = pick_commit()
    if target then
      submit(target)
    end
  end
end

maki.api.register_command({
  name = "/review",
  description = "Review code changes in a reviewer thread",
  nargs = "*",
  handler = open,
})

maki.keymap.set("n", "<A-r>", open, { desc = "Review code changes" })
