# Subagent C — Adjacent Analysis

Read the function at `<file>:<line-range>`, then find its **direct caller**
(one major function up) and **direct callee** (one major function down).
Transitions through helpers/glue code don't count as levels.
Read those functions too.

For each interface boundary, check:
- Does the caller pass arguments that match what the function expects?
- Does the function return values that the caller handles correctly?
- Are error types/propagation consistent across the boundary?
- Are there implicit contracts (non-null, bounded range, initialization order,
  cleanup responsibility) that are assumed but not enforced by types?
- Can any dependency this function calls panic on the inputs supplied?
- Is there a resource leak risk (file handle, mutex, connection) on any
  error path?

Return:
1. A list of boundary mismatches or contract gaps found
2. If none, state "No boundary issues found"
3. Suggested contract tests for any mismatches found

Do NOT include inlined code — use file:line-range references only.
