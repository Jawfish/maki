local helpers = require("review_helpers")

local failures = {}

local function case(name, fn)
  local ok, err = pcall(fn)
  if not ok then
    table.insert(failures, name .. ": " .. tostring(err))
  end
end

local function eq(actual, expected, msg)
  if actual ~= expected then
    error((msg or "") .. "\nexpected: " .. tostring(expected) .. "\n  actual: " .. tostring(actual))
  end
end

local function contains(haystack, needle)
  assert(haystack:find(needle, 1, true), "expected to find " .. needle .. " in:\n" .. haystack)
end

case("uncommitted_target_mentions_untracked_files", function()
  local prompt, hint = helpers.resolve_target({ kind = "uncommitted" })
  contains(prompt, "untracked")
  eq(hint, "current changes")
end)

case("base_branch_target_uses_merge_base", function()
  local prompt, hint = helpers.resolve_target({ kind = "base", branch = "main", merge_base = "abc1234" })
  contains(prompt, "git diff abc1234")
  eq(hint, "changes against 'main'")
end)

case("base_branch_target_without_merge_base_falls_back", function()
  local prompt = helpers.resolve_target({ kind = "base", branch = "main" })
  contains(prompt, "git merge-base HEAD main")
end)

case("commit_target_keeps_title_and_shortens_sha", function()
  local prompt, hint = helpers.resolve_target({ kind = "commit", sha = "0123456789ab", title = "fix parser" })
  contains(prompt, "git show 0123456789ab")
  eq(hint, "commit 0123456: fix parser")
end)

case("custom_target_is_trimmed", function()
  local prompt, hint = helpers.resolve_target({ kind = "custom", instructions = "  check the auth flow  " })
  eq(prompt, "check the auth flow")
  eq(hint, "check the auth flow")
end)

case("parse_commits_splits_sha_and_title", function()
  local commits = helpers.parse_commits("abc123\tadd review plugin\ndef456\tfix\ttabs in title\n")
  eq(#commits, 2)
  eq(commits[1].sha, "abc123")
  eq(commits[1].title, "add review plugin")
  eq(commits[2].title, "fix\ttabs in title")
end)

case("parse_branches_drops_blank_lines", function()
  local branches = helpers.parse_branches("main\n\n  feature/x  \n")
  eq(#branches, 2)
  eq(branches[1], "main")
  eq(branches[2], "feature/x")
end)

case("format_result_sorts_findings_by_priority", function()
  local text = helpers.format_result({
    verdict = "incorrect",
    explanation = "One blocking issue.",
    findings = {
      { title = "Nit", body = "minor", priority = 3, file = "a.rs", start_line = 1, end_line = 2 },
      { title = "Panic on empty input", body = "boom", priority = 0, file = "b.rs", start_line = 9, end_line = 9 },
    },
  }, "current changes")
  contains(text, "Review of current changes: incorrect")
  contains(text, "### [P0] Panic on empty input")
  contains(text, "`b.rs:9-9`")
  assert(text:find("[P0]", 1, true) < text:find("[P3]", 1, true), "P0 must render before P3")
end)

case("format_result_reports_a_clean_review", function()
  local text = helpers.format_result({ verdict = "correct", explanation = "Looks fine.", findings = {} }, "commit abc")
  contains(text, "No findings.")
end)

if #failures > 0 then
  error(#failures .. " case(s) failed:\n\n" .. table.concat(failures, "\n\n"))
end
