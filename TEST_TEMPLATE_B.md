# Subagent B — Control Flow Extraction

Read the function at `<file>:<line-range>`, then extract ALL control flow
entry and exit points that need to be tested.

For each path, list:
- The conditions leading to it
- The order of operations (what happens before what)
- State transitions
- Every early return and error path

Include:
- Normal success path (happy path)
- Early return paths
- Error return paths
- All branches (match arms, if-else, guards, ? operators)
- The order in which guards are evaluated
- Loops and their exit conditions
- Resource cleanup on each path (files, connections, mutex locks)

Be exhaustive. Do NOT include inlined code — use file:line-range references only.
