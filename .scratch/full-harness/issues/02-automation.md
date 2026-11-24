# 02: Unattended GitHub maintenance
Blocked by: baseline; CLI integration after 01
Status: ready-for-agent
Own automation.rs plus automation integration tests/examples only. Implement stable GitHub ingestion IDs, durable claims and recovery, real git worktrees (no copy fallback hiding failure), callback-based task execution and verification, gated local commits and shipping permissions, poll/once service operations. Acceptance source ../spec.md items 6-8. No edits to main.rs/lib.rs or other modules. No build/test/lint/formatter mid-flight.
