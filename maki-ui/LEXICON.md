# UI lexicon

One term per concept, one marker per lifecycle state. Anything the user reads
in the TUI follows this note. Docs and log lines follow it too when they talk
about the same thing.

## Lifecycle states

`components/marker.rs` is the only place that decides how a state looks. Never
hardcode a glyph, a word or a color for a state anywhere else. Color is never
the only signal: every state carries a distinct glyph and a word.

| State | Glyph | Word | Theme style |
| --- | --- | --- | --- |
| `Queued` | `◌` | queued | `queue` |
| `Running` | animated spinner | running | `spinner` |
| `Done` | `✔` | done | `tool_success` |
| `Failed` | `✘` | failed | `tool_error` |
| `Cancelled` | `⊘` | cancelled | `tool_dim` |
| `NeedsAttention` | `▲` | attention | `status_notice` |

Use `State::glyph_span` where space is tight and a nearby label already names
the state (tool headers, queue rows). Use `State::label` or
`State::label_spans` where the state is the whole message (run finished,
status bar, retries).

Lua plugins cannot call the module, so they mirror it through theme style
names (`plugins/batch/init.lua`). Keep them in sync when the table changes.

## Words

| Concept | Use | Do not use |
| --- | --- | --- |
| A conversation with the agent | session | chat, thread, conversation |
| Granting a tool call | allow | approve, accept, yes |
| Refusing a tool call | deny | reject, no |
| Ending work in flight | cancel | stop, abort, interrupt, kill |
| Trying a failed request again | retry | re-run, again |
| A capability the agent calls | tool | action, function, command |
| Tokens the model can still see | context | window, memory |

Notes:

- "cancel" is both the verb the user performs and the state the run lands in
  ("cancelled"). A prompt that dismisses without acting also says "Cancel".
- "session" is the user-facing word; `chat` survives as an internal type name
  only, and should not leak into strings.
- "command" stays reserved for slash commands and shell commands, so a tool is
  always a "tool".
