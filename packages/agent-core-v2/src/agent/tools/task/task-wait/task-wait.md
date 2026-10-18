Block until one background task settles (completed / failed / timed out / killed / lost), or until the timeout expires.

Discouraged — background tasks notify automatically on completion, so the default workflow is to end the turn and let the result arrive as a message. Use TaskWait only when the next step truly depends on this result and there is no other useful work to do. When the wait times out, the honest partial output is returned with a "still running" marking — end the turn and the completion will still be delivered automatically. To wait for several tasks, call TaskWait multiple times in one parallel block.

Parameters:
- task_id: The background task ID to wait for.
- timeout: Maximum seconds to wait (default 30, at most 3600).
