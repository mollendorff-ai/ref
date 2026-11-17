//! Running work on a task of its own, so a panic in it stays there.

use std::future::Future;

/// Run `work` on its own task. A panic in it becomes `Err` with the panic
/// message instead of unwinding the caller, so one bad page cannot take down
/// a whole run or the MCP server's tool call. When the caller stops waiting
/// (a run that ends early), the task is cancelled too (ADR-005 rule 7).
///
/// # Errors
/// Returns the panic message when `work` panicked or its task was cancelled.
pub async fn isolated<T: Send + 'static>(
    work: impl Future<Output = T> + Send + 'static,
) -> Result<T, String> {
    let mut task = CancelOnDrop(tokio::spawn(work));
    // Not a panic means cancelled, which needs the runtime to shut down while
    // this call waits, and then this call is dropped too
    (&mut task.0).await.map_err(|e| {
        e.try_into_panic()
            .map_or_else(|_| "cancelled".into(), |p| panic_message(&*p))
    })
}

/// A task that is cancelled when its handle is dropped
struct CancelOnDrop<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for CancelOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The text of a panic: the message given to `panic!`, or "panicked"
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(ToString::to_string)
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "panicked".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn work_that_finishes_gives_its_value() {
        assert_eq!(isolated(async { 7 }).await, Ok(7));
    }

    /// Sets its flag when dropped: a cancelled task drops what it holds
    struct DroppedFlag(std::sync::Arc<std::sync::atomic::AtomicBool>);

    impl Drop for DroppedFlag {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn work_is_cancelled_when_the_caller_stops_waiting() {
        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = DroppedFlag(std::sync::Arc::clone(&dropped));
        // Work that would never end on its own
        let work = isolated(async move {
            let _held = flag;
            std::future::pending::<()>().await;
        });
        let gave_up = tokio::time::timeout(std::time::Duration::from_millis(50), work).await;
        assert!(gave_up.is_err());
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let cancelled = dropped.load(std::sync::atomic::Ordering::SeqCst);
        assert!(cancelled, "the work kept running");
    }

    #[tokio::test]
    async fn a_panic_becomes_its_message() {
        let literal = isolated(async { panic!("bad page") }).await;
        assert_eq!(literal, Err::<(), _>("bad page".to_string()));

        let page = 3;
        let formatted = isolated(async move { panic!("bad page {page}") }).await;
        assert_eq!(formatted, Err::<(), _>("bad page 3".to_string()));

        let other = isolated(async { std::panic::panic_any(42_u8) }).await;
        assert_eq!(other, Err::<(), _>("panicked".to_string()));
    }
}
