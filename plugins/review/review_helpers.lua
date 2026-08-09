-- Prompt wording and result formatting, kept apart from init.lua so the
-- pure string work is testable without an agent context.

local M = {}

local UNCOMMITTED_PROMPT =
  "Review the current code changes (staged, unstaged, and untracked files) and provide prioritized findings."
local BASE_BRANCH_PROMPT = "Review the code changes against the base branch '%s'. The merge base for this comparison "
  .. "is %s. Run `git diff %s` to see what would merge into %s. Provide prioritized, actionable findings."
local BASE_BRANCH_FALLBACK_PROMPT = "Review the code changes against the base branch '%s'. Find the merge base with "
  .. "`git merge-base HEAD %s`, then diff against that commit. Provide prioritized, actionable findings."
local COMMIT_PROMPT = "Review the code changes introduced by commit %s. Inspect it with `git show %s`. Provide "
  .. "prioritized, actionable findings."
local COMMIT_TITLE_PROMPT = 'Review the code changes introduced by commit %s ("%s"). Inspect it with `git show %s`. '
  .. "Provide prioritized, actionable findings."

local UNCOMMITTED_HINT = "current changes"
local BASE_BRANCH_HINT = "changes against '%s'"
local COMMIT_HINT = "commit %s"
local COMMIT_TITLE_HINT = "commit %s: %s"
local SHORT_SHA_LEN = 7

local NO_FINDINGS = "No findings."
local SUMMARY = "Review of %s: %s"
local FINDING_HEADING = "### [P%d] %s"
local FINDING_LOCATION = "`%s:%d-%d`"
local LOWEST_PRIORITY = 3

local function trim(text)
  return (text or ""):match("^%s*(.-)%s*$")
end

local function short_sha(sha)
  return sha:sub(1, SHORT_SHA_LEN)
end

-- Turns a picked target into the reviewer's opening message plus the short
-- label the UI and the parent conversation use to name the run.
function M.resolve_target(target)
  if target.kind == "uncommitted" then
    return UNCOMMITTED_PROMPT, UNCOMMITTED_HINT
  end
  if target.kind == "base" then
    local hint = BASE_BRANCH_HINT:format(target.branch)
    if target.merge_base then
      return BASE_BRANCH_PROMPT:format(target.branch, target.merge_base, target.merge_base, target.branch), hint
    end
    return BASE_BRANCH_FALLBACK_PROMPT:format(target.branch, target.branch), hint
  end
  if target.kind == "commit" then
    if target.title then
      return COMMIT_TITLE_PROMPT:format(target.sha, target.title, target.sha),
        COMMIT_TITLE_HINT:format(short_sha(target.sha), target.title)
    end
    return COMMIT_PROMPT:format(target.sha, target.sha), COMMIT_HINT:format(short_sha(target.sha))
  end
  local instructions = trim(target.instructions)
  return instructions, instructions
end

-- `git log --pretty=format:%h%x09%s` lines into { sha, title } records.
function M.parse_commits(stdout)
  local commits = {}
  for _, line in ipairs(maki.split(stdout or "", "\n")) do
    local sha, title = line:match("^(%S+)\t(.*)$")
    if sha then
      commits[#commits + 1] = { sha = sha, title = title }
    end
  end
  return commits
end

function M.parse_branches(stdout)
  local branches = {}
  for _, line in ipairs(maki.split(stdout or "", "\n")) do
    local branch = trim(line)
    if branch ~= "" then
      branches[#branches + 1] = branch
    end
  end
  return branches
end

-- Markdown, because the parent conversation keeps this text and the user
-- reads the same block in the transcript.
function M.format_result(result, hint)
  local findings = result.findings or {}
  table.sort(findings, function(a, b)
    return (a.priority or LOWEST_PRIORITY) < (b.priority or LOWEST_PRIORITY)
  end)

  local lines = { SUMMARY:format(hint, result.verdict) }
  local explanation = trim(result.explanation)
  if explanation ~= "" then
    lines[#lines + 1] = ""
    lines[#lines + 1] = explanation
  end
  if #findings == 0 then
    lines[#lines + 1] = ""
    lines[#lines + 1] = NO_FINDINGS
  end
  for _, finding in ipairs(findings) do
    lines[#lines + 1] = ""
    lines[#lines + 1] = FINDING_HEADING:format(finding.priority or LOWEST_PRIORITY, finding.title)
    lines[#lines + 1] = FINDING_LOCATION:format(finding.file, finding.start_line, finding.end_line)
    lines[#lines + 1] = ""
    lines[#lines + 1] = trim(finding.body)
  end
  return table.concat(lines, "\n")
end

return M
