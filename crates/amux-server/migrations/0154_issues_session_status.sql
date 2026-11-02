-- board-drive asks each lane for its cards in one status (todo for pickup and
-- the queue-shape count, backlog for the drain). On idx_issues_session alone
-- it fetched every card the lane ever had from the table to test `status`:
-- 4,122 rows to find 82 todo cards on the busiest lane, each fetch a page read
-- on a 15.8 GB database. A 40 s sample put those fetches under lane_loop's
-- 1-3 s ticks (AF-1120, AF-1006).
CREATE INDEX IF NOT EXISTS idx_issues_session_status ON issues(session, status);
