//! Waits that count only the time the machine is awake (#243). On Windows, `Instant`, and
//! so tokio's timers, go on while the machine is in standby: a wait that should have ended
//! while it slept ends the moment it wakes, before its peers have reconnected or its model
//! has gone on. These waits go in short ticks, and a tick that ends much later than asked
//! for (the machine was suspended) doesn't count.

use std::future::Future;
use std::time::Duration;

use tokio::time::Instant;

/// The longest tick: a suspension costs a wait at most this much of its awake time.
const TICK: Duration = Duration::from_secs(10);

/// How much later than asked for a tick may end and still count (a busy machine's timers
/// run a little late). A tick that ends later than that was suspended.
const LATE: Duration = Duration::from_secs(15);

/// The awake time a tick that was to last `asked`, and lasted `took`, counts.
fn counted(asked: Duration, took: Duration) -> Duration {
    if took > asked + LATE {
        Duration::ZERO
    } else {
        took
    }
}

/// Waits until `wait` of awake time has passed.
pub async fn sleep(wait: Duration) {
    let mut left = wait;
    while !left.is_zero() {
        let asked = left.min(TICK);
        let start = Instant::now();
        tokio::time::sleep(asked).await;
        left = left.saturating_sub(counted(asked, start.elapsed()));
    }
}

/// What `work` gives, or None (and `work` is dropped) if `limit` of awake time passes first.
pub async fn timeout<T>(limit: Duration, work: impl Future<Output = T>) -> Option<T> {
    tokio::select! {
        out = work => Some(out),
        () = sleep(limit) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOURS_7: Duration = Duration::from_secs(7 * 60 * 60);

    #[tokio::test(start_paused = true)]
    async fn time_the_machine_was_suspended_doesnt_count() {
        let start = Instant::now();
        let wait = tokio::spawn(sleep(Duration::from_secs(60)));
        tokio::task::yield_now().await;
        // Seven hours in standby during its first tick: that tick counts nothing.
        tokio::time::advance(HOURS_7).await;
        assert!(!wait.is_finished());
        wait.await.unwrap();
        assert_eq!(start.elapsed(), HOURS_7 + Duration::from_secs(60));

        // A tick that runs a little late counts as it ran.
        assert_eq!(
            counted(TICK, TICK + Duration::from_secs(3)),
            TICK + Duration::from_secs(3)
        );
        assert_eq!(
            counted(TICK, TICK + LATE + Duration::from_secs(1)),
            Duration::ZERO
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_timeout_counts_only_awake_time_and_lets_work_that_ends_in_time_finish() {
        let start = Instant::now();
        let never = std::future::pending::<()>();
        let limit = tokio::spawn(timeout(Duration::from_secs(30), never));
        tokio::task::yield_now().await;
        tokio::time::advance(HOURS_7).await;
        assert!(!limit.is_finished());
        assert_eq!(limit.await.unwrap(), None);
        assert_eq!(start.elapsed(), HOURS_7 + Duration::from_secs(30));

        let quick = async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            7
        };
        assert_eq!(timeout(Duration::from_secs(30), quick).await, Some(7));
    }
}
