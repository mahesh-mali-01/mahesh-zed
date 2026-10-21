use crate::review_store::ReviewDb;
use futures::{FutureExt as _, future::Shared};
use gpui::{App, AppContext as _, Global, SharedString, Task};

pub(crate) type Recovery = Shared<Task<Result<(), SharedString>>>;

struct OutboxRecovery(Recovery);
impl Global for OutboxRecovery {}

// Multiple restored tabs must await the same recovery. Running it again after
// dispatch starts would mistake a live request for an interrupted operation.
pub(crate) fn ready(db: ReviewDb, cx: &mut App) -> Recovery {
    if let Some(recovery) = cx.try_global::<OutboxRecovery>() {
        return recovery.0.clone();
    }
    let recovery = cx
        .background_spawn(async move {
            db.recover_sending_operations()
                .await
                .map(|_| ())
                .map_err(|error| {
                    log::error!("GitHub review outbox recovery failed: {error:#}");
                    SharedString::from("Review recovery failed. Restart the editor before posting.")
                })
        })
        .shared();
    cx.set_global(OutboxRecovery(recovery.clone()));
    recovery
}

// The app owns delivery, not a composer, tab or window. Shared tasks deduplicate
// concurrent attempts at the same operation; database claims remain the
// authoritative cross-restart lease and unknown POSTs are never replayed.
pub(crate) type Delivery = Shared<Task<Result<crate::review::ReviewOperation, SharedString>>>;
#[derive(Default)]
struct OutboxDeliveries {
    operations: std::collections::HashMap<String, Delivery>,
    lanes: std::collections::HashMap<String, Delivery>,
}
impl Global for OutboxDeliveries {}

pub(crate) fn is_in_flight(id: &str, cx: &App) -> bool {
    cx.try_global::<OutboxDeliveries>()
        .and_then(|deliveries| deliveries.operations.get(id))
        .is_some_and(|delivery| delivery.peek().is_none())
}

pub(crate) fn deliver(
    lane: String,
    id: String,
    future: impl std::future::Future<Output = anyhow::Result<crate::review::ReviewOperation>>
    + Send
    + 'static,
    cx: &mut App,
) -> Delivery {
    if let Some(delivery) = cx
        .try_global::<OutboxDeliveries>()
        .and_then(|deliveries| deliveries.operations.get(&id))
        .filter(|delivery| delivery.peek().is_none())
    {
        return delivery.clone();
    }
    let prior = cx
        .try_global::<OutboxDeliveries>()
        .and_then(|deliveries| deliveries.lanes.get(&lane))
        .cloned();
    let task = cx
        .background_spawn(async move {
            if let Some(prior) = prior {
                // Serialize writes against the same pinned PR session. In
                // particular pending-review creation must finish before another
                // Add-to-review discovers or creates its dependency.
                let _ = prior.await;
            }
            future.await.map_err(|error| {
                SharedString::from(format!("Saved operation delivery stopped: {error:#}"))
            })
        })
        .shared();
    if cx.try_global::<OutboxDeliveries>().is_none() {
        cx.set_global(OutboxDeliveries::default());
    }
    let deliveries = cx.global_mut::<OutboxDeliveries>();
    deliveries
        .operations
        .retain(|_, delivery| delivery.peek().is_none());
    deliveries
        .lanes
        .retain(|_, delivery| delivery.peek().is_none());
    deliveries.operations.insert(id, task.clone());
    deliveries.lanes.insert(lane, task.clone());
    // Poll completion independently of every UI observer. Without this watcher
    // a closed tab could leave a finished Shared<Task> looking in-flight until
    // a later caller polls it, preventing a legitimate explicit retry.
    let watcher = task.clone();
    cx.background_spawn(async move {
        let _ = watcher.await;
    })
    .detach();
    task
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Context as _, Result};
    use db::sqlez::thread_safe_connection::ThreadSafeConnection;
    use gpui::TestAppContext;

    #[gpui::test]
    async fn review_outbox_recovery_is_shared_and_does_not_repeat(cx: &mut TestAppContext) {
        assert_shared_recovery(cx)
            .await
            .expect("shared outbox recovery must complete exactly once");
    }

    #[gpui::test]
    async fn completed_delivery_can_be_resumed_and_inflight_duplicates_share(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (release, gate) = futures::channel::oneshot::channel::<()>();
        let first = cx.update(|cx| {
            deliver(
                "lane".into(),
                "same-operation".into(),
                {
                    let calls = calls.clone();
                    async move {
                        calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        gate.await.unwrap();
                        anyhow::bail!("definitely unsent")
                    }
                },
                cx,
            )
        });
        let duplicate = cx.update(|cx| {
            deliver(
                "lane".into(),
                "same-operation".into(),
                async { panic!("inflight duplicate must not dispatch") },
                cx,
            )
        });
        release.send(()).unwrap();
        assert!(first.await.is_err());
        assert!(duplicate.await.is_err());
        let resumed = cx.update(|cx| {
            deliver(
                "lane".into(),
                "same-operation".into(),
                {
                    let calls = calls.clone();
                    async move {
                        calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        anyhow::bail!("resumed dispatch actually ran")
                    }
                },
                cx,
            )
        });
        assert!(
            resumed
                .await
                .unwrap_err()
                .contains("resumed dispatch actually ran")
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 2);
    }

    #[gpui::test]
    async fn delivery_completes_and_releases_lease_after_all_callers_close(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        let completed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (release, gate) = futures::channel::oneshot::channel::<()>();
        let delivery = cx.update(|cx| {
            deliver(
                "closed-tab-lane".into(),
                "closed-tab-operation".into(),
                {
                    let completed = completed.clone();
                    async move {
                        gate.await.unwrap();
                        completed.store(true, std::sync::atomic::Ordering::SeqCst);
                        anyhow::bail!("definitely unsent after closed caller")
                    }
                },
                cx,
            )
        });
        drop(delivery);
        release.send(()).unwrap();
        cx.executor().run_until_parked();
        assert!(completed.load(std::sync::atomic::Ordering::SeqCst));
        assert!(
            !cx.update(|cx| is_in_flight("closed-tab-operation", cx)),
            "application owner must observe completion even without any open view"
        );
    }

    async fn assert_shared_recovery(cx: &mut TestAppContext) -> Result<()> {
        cx.executor().allow_parking();
        let directory = tempfile::tempdir()?;
        let connection = ThreadSafeConnection::builder::<ReviewDb>(
            directory
                .path()
                .join("review.sqlite")
                .to_str()
                .context("database path")?,
            true,
        )
        .with_connection_initialize_query("PRAGMA foreign_keys=ON;")
        .build()
        .await?;
        let db = ReviewDb::from_connection(connection)?;
        let first = cx.update(|cx| ready(db.clone(), cx));
        let second = cx.update(|cx| ready(db.clone(), cx));
        first
            .await
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        second
            .await
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        // Poisoning the table after recovery detects a second recovery query:
        // later tabs must retain the completed successful gate.
        db.write(|connection| -> Result<()> {
            connection.exec("DROP TABLE review_operations")?()?;
            Ok(())
        })
        .await?;
        cx.update(|cx| ready(db, cx))
            .await
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        Ok(())
    }
}
