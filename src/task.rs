//! Running work on a task of its own, so a panic in it stays there.

use std::future::Future;

/// Run `work` on its own task. A panic in it becomes `Err` with the panic
/// message instead of unwinding the caller, so one bad page cannot take down
/// a whole run or the MCP server's tool call.
///
/// # Errors
/// Returns the panic message when `work` panicked or its task was cancelled.
pub async fn isolated<T: Send + 'static>(
    work: impl Future<Output = T> + Send + 'static,
) -> Result<T, String> {
    // Not a panic means cancelled, which needs the runtime to shut down while
    // this call waits, and then this call is dropped too
    tokio::spawn(work).await.map_err(|e| {
        e.try_into_panic()
            .map_or_else(|_| "cancelled".into(), |p| panic_message(&*p))
    })
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
