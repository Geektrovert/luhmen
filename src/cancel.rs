use tokio::sync::watch;

/// A permanent cancellation state that also wakes current and future waiters.
#[derive(Clone, Debug)]
pub struct Cancellation {
    state: watch::Sender<bool>,
}

impl Default for Cancellation {
    fn default() -> Self {
        Self::new()
    }
}

impl Cancellation {
    pub fn new() -> Self {
        Self {
            state: watch::Sender::new(false),
        }
    }

    pub fn cancel(&self) {
        // Unlike send(), this retains the state even before a waiter subscribes.
        self.state.send_replace(true);
    }

    pub fn is_cancelled(&self) -> bool {
        *self.state.borrow()
    }

    pub async fn cancelled(&self) {
        let mut state = self.state.subscribe();
        while !*state.borrow_and_update() {
            if state.changed().await.is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test(flavor = "current_thread")]
    async fn cancellation_reaches_existing_and_late_waiters() {
        let cancellation = Cancellation::new();
        let early = cancellation.clone();
        let waiter = tokio::spawn(async move { early.cancelled().await });
        tokio::task::yield_now().await;
        cancellation.cancel();
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), cancellation.cancelled())
            .await
            .unwrap();
        assert!(cancellation.is_cancelled());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelling_without_a_receiver_is_retained() {
        let cancellation = Cancellation::new();
        cancellation.cancel();
        cancellation.cancel();
        tokio::time::timeout(Duration::from_secs(1), cancellation.cancelled())
            .await
            .unwrap();
    }
}
