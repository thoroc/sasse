-- Keep the reason a candidate failed after its log has been pruned.
--
-- The byte budget on the log directory means a failure's transcript is
-- eventually removed. Its last lines are small enough to keep on the row
-- instead, so the verdict, the command and the actual reason survive
-- indefinitely at a couple of kilobytes each.
--
-- Only failures get one. A passing run has nothing to explain.

ALTER TABLE run ADD COLUMN tail TEXT;
