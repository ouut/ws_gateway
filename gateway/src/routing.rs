//! Routing core — the central message dispatcher for the smart gateway.
//!
//! ## Architecture
//!
//! - **Room registry**: `DashMap<RoomID, DashMap<UserID, BoundedSender<Bytes>>>`
//!   — segmented locking so different rooms never contend.  Wrapped in `Arc`
//!   so the router is cheaply cloneable and can be passed into spawned tasks.
//! - **Bounded channels** with per-packet-type backpressure:
//!   - `RawMotion` (real-time priority): evicts oldest frame when the
//!     channel is full, then inserts the newest (`send_drop_oldest`).
//!   - `SystemCmd` (reliability priority): `try_send` failure triggers a
//!     WARN log, up to 50 ms of retry with short awaits, and a forced
//!     disconnect on final failure.
//!   - `AiEvent` / `Heartbeat`: best-effort `try_send`; silently dropped
//!     with a DEBUG log when the channel is full.
//! - **Duplicate connections**: inserting a sender for the same
//!   (room, user) pair evicts the old sender and emits a WARN.
//! - **Room cleanup**: the last user leaving a room destroys the room
//!   entry automatically.

use bytes::Bytes;
use dashmap::DashMap;
use gateway_protocol::{PacketHeader, PacketType, TargetType};
use log::{debug, warn};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

// ---------------------------------------------------------------------------
// Type aliases
// ---------------------------------------------------------------------------

type RoomID = String;
type UserID = String;

// ---------------------------------------------------------------------------
// Bounded MPSC channel with optional oldest-eviction
// ---------------------------------------------------------------------------

/// Shared inner state of the bounded channel.
struct ChannelInner<T> {
    queue: Mutex<VecDeque<T>>,
    capacity: usize,
    /// Woken every time an item is pushed (even evicting pushes).
    notify: Notify,
}

/// Sender half of the bounded channel.
///
/// Cheaply cloneable (`Arc`-backed). Supports `try_send` (returns `Err` when
/// full) and `send_drop_oldest` (evicts the front of the queue to make room).
pub struct BoundedSender<T> {
    inner: Arc<ChannelInner<T>>,
}

impl<T> Clone for BoundedSender<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for BoundedSender<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoundedSender")
            .field("capacity", &self.inner.capacity)
            .finish_non_exhaustive()
    }
}

/// Receiver half of the bounded channel.
///
/// Only one receiver may exist; after it is dropped further sends succeed (the
/// items are simply discarded because the shared state remains alive).
pub struct BoundedReceiver<T> {
    inner: Arc<ChannelInner<T>>,
}

impl<T> Drop for BoundedReceiver<T> {
    fn drop(&mut self) {
        // Wake any pending `recv` so they see the queue is empty and the
        // receiver (which they hold a reference to) is gone.
        self.inner.notify.notify_one();
    }
}

/// Create a new bounded channel with the given capacity.
pub fn bounded<T>(capacity: usize) -> (BoundedSender<T>, BoundedReceiver<T>) {
    let inner = Arc::new(ChannelInner {
        queue: Mutex::new(VecDeque::with_capacity(capacity)),
        capacity,
        notify: Notify::new(),
    });
    (
        BoundedSender {
            inner: Arc::clone(&inner),
        },
        BoundedReceiver { inner },
    )
}

impl<T> BoundedSender<T> {
    /// Non-blocking send.  Returns `Err(item)` when the channel is full.
    pub fn try_send(&self, item: T) -> Result<(), T> {
        let mut q = self.inner.queue.lock().unwrap();
        if q.len() < self.inner.capacity {
            q.push_back(item);
            self.inner.notify.notify_one();
            Ok(())
        } else {
            Err(item)
        }
    }

    /// Send, evicting the **oldest** item if the channel is already full.
    /// Always succeeds.
    pub fn send_drop_oldest(&self, item: T) {
        let mut q = self.inner.queue.lock().unwrap();
        if q.len() >= self.inner.capacity {
            q.pop_front();
        }
        q.push_back(item);
        self.inner.notify.notify_one();
    }

    /// Try to send with a per-call timeout.  Retries with short sleeps until
    /// the item is accepted or `timeout` elapses.  Returns `Err(item)` on
    /// timeout.
    pub async fn send_timeout(&self, mut item: T, timeout: Duration) -> Result<(), T> {
        let start = tokio::time::Instant::now();
        loop {
            match self.try_send(item) {
                Ok(()) => return Ok(()),
                Err(returned) => {
                    item = returned;
                    if start.elapsed() >= timeout {
                        return Err(item);
                    }
                    // Short back-off before retrying.
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }
        }
    }
}

impl<T> BoundedReceiver<T> {
    /// Receive the next item.  Returns `None` when all senders have been
    /// dropped AND the queue is empty.
    pub async fn recv(&mut self) -> Option<T> {
        loop {
            // Fast path: grab an item under the lock.
            {
                let mut q = self.inner.queue.lock().unwrap();
                if let Some(item) = q.pop_front() {
                    return Some(item);
                }
                // If the Arc strong count is 1, only this receiver (and the
                // shared inner) remain — no senders left.
                if Arc::strong_count(&self.inner) == 1 {
                    return None;
                }
            }
            // Slow path: wait for a push notification.
            self.inner.notify.notified().await;
        }
    }
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

/// The routing core.
///
/// Thread-safe and cheaply cloneable (all state is behind `Arc`-backed
/// concurrent maps).
#[derive(Clone)]
pub struct Router {
    rooms: Arc<DashMap<RoomID, DashMap<UserID, BoundedSender<Bytes>>>>,
}

impl Router {
    /// Create an empty router.
    pub fn new() -> Self {
        Self {
            rooms: Arc::new(DashMap::new()),
        }
    }

    /// Register a new connection.
    ///
    /// If a sender already exists for `(room_id, user_id)` the old sender is
    /// dropped (which will cause the corresponding receiver task to notice the
    /// closure) and a `WARN` is logged.
    pub fn register(&self, room_id: &str, user_id: &str, tx: BoundedSender<Bytes>) {
        let room = self
            .rooms
            .entry(room_id.to_string())
            .or_insert_with(DashMap::new);

        if let Some(_old) = room.insert(user_id.to_string(), tx) {
            warn!(
                "Duplicate connection kicked: room={room_id} user={user_id}"
            );
        }
    }

    /// Remove a connection.
    ///
    /// If the room becomes empty after removal the whole room entry is
    /// destroyed (DEBUG log).
    pub fn unregister(&self, room_id: &str, user_id: &str) {
        let room_id = room_id.to_string();
        let user_id = user_id.to_string();
        Self::unregister_internal(&self.rooms, &room_id, &user_id);
    }

    /// Static helper so spawned tasks can unregister without holding a full
    /// `Router` reference.
    fn unregister_internal(
        rooms: &DashMap<RoomID, DashMap<UserID, BoundedSender<Bytes>>>,
        room_id: &str,
        user_id: &str,
    ) {
        let should_cleanup = {
            if let Some(room) = rooms.get(room_id) {
                room.remove(user_id);
                room.is_empty()
            } else {
                return;
            }
        };

        if should_cleanup {
            if let Some((_, _room)) = rooms.remove_if(room_id, |_, r| r.is_empty()) {
                debug!("Room destroyed (no active connections): room={room_id}");
            }
        }
    }

    /// Route a decoded packet to its destination(s).
    ///
    /// Called by the network layer after `PacketHeader::decode` succeeds.
    pub async fn route(&self, header: PacketHeader, payload: Bytes) {
        let room_id = header.room_id_str();

        // Resolve the target room.
        let room = match self.rooms.get(room_id) {
            Some(r) => r,
            None => {
                debug!(
                    "Packet dropped: room not found (room={room_id} \
                     user={})",
                    header.user_id_str()
                );
                return;
            }
        };

        match header.target_type {
            TargetType::Broadcast => {
                // Snapshot all (user, sender) pairs, then dispatch.
                let targets: Vec<(String, BoundedSender<Bytes>)> = room
                    .iter()
                    .map(|entry| (entry.key().clone(), entry.value().clone()))
                    .collect();

                for (target_user, tx) in targets {
                    self.dispatch(
                        &header,
                        &payload,
                        &target_user,
                        &tx,
                    );
                }
            }
            TargetType::Unicast => {
                let target_user = header.user_id_str();

                match room.get(target_user) {
                    Some(tx) => {
                        self.dispatch(
                            &header,
                            &payload,
                            target_user,
                            &tx,
                        );
                    }
                    None => {
                        debug!(
                            "Packet dropped: user not in room \
                             (room={room_id} user={target_user})"
                        );
                    }
                }
            }
        }
    }

    // -- internal helpers ---------------------------------------------------

    /// Apply per-packet-type backpressure policy and deliver `payload` to a
    /// single user.
    fn dispatch(
        &self,
        header: &PacketHeader,
        payload: &Bytes,
        target_user: &str,
        tx: &BoundedSender<Bytes>,
    ) {
        match header.packet_type {
            PacketType::RawMotion => {
                // Real-time priority: drop oldest frame, insert newest.
                tx.send_drop_oldest(payload.clone());
            }
            PacketType::SystemCmd => {
                // Reliability priority: retry with bounded await.
                match tx.try_send(payload.clone()) {
                    Ok(()) => { /* delivered */ }
                    Err(returned) => {
                        warn!(
                            "SystemCmd channel full, retrying... \
                             (room={} user={})",
                            header.room_id_str(),
                            target_user
                        );
                        // Spawn a task that retries with a 50 ms budget.
                        // On final failure it unregisters the connection.
                        let rooms = Arc::clone(&self.rooms);
                        let tx = tx.clone();
                        let room_id = header.room_id_str().to_string();
                        let user_id = target_user.to_string();
                        tokio::spawn(async move {
                            match tx
                                .send_timeout(returned, Duration::from_millis(50))
                                .await
                            {
                                Ok(()) => { /* delivered after retry */ }
                                Err(_) => {
                                    warn!(
                                        "SystemCmd send failed after 50 ms retry, \
                                         disconnecting (room={room_id} user={user_id})"
                                    );
                                    Router::unregister_internal(
                                        &rooms,
                                        &room_id,
                                        &user_id,
                                    );
                                }
                            }
                        });
                    }
                }
            }
            // AiEvent, Heartbeat — best effort.
            _ => {
                if tx.try_send(payload.clone()).is_err() {
                    debug!(
                        "Packet dropped (channel full): type={:?} room={} user={}",
                        header.packet_type,
                        header.room_id_str(),
                        target_user
                    );
                }
            }
        }
    }

    /// Number of active rooms.
    #[cfg(test)]
    pub fn room_count(&self) -> usize {
        self.rooms.len()
    }

    /// Total number of connected users across all rooms.
    #[cfg(test)]
    pub fn user_count(&self) -> usize {
        self.rooms.iter().map(|entry| entry.value().len()).sum()
    }
}

impl Default for Router {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use gateway_protocol::PacketHeader;

    // -- channel tests -------------------------------------------------------

    /// Basic bounded channel send / recv.
    #[tokio::test]
    async fn channel_send_recv() {
        let (tx, mut rx) = bounded::<u32>(4);
        for i in 0..4 {
            tx.try_send(i).unwrap();
        }
        assert!(tx.try_send(99).is_err()); // full
        tx.send_drop_oldest(99); // evicts 0, inserts 99
        assert_eq!(rx.recv().await, Some(1)); // 0 was dropped
        assert_eq!(rx.recv().await, Some(2));
        assert_eq!(rx.recv().await, Some(3));
        assert_eq!(rx.recv().await, Some(99));
        drop(tx);
        assert_eq!(rx.recv().await, None);
    }

    /// `send_timeout` eventually succeeds when the receiver drains.
    #[tokio::test]
    async fn channel_send_timeout() {
        let (tx, mut rx) = bounded::<u32>(2);
        tx.try_send(1).unwrap();
        tx.try_send(2).unwrap();

        // Channel is full; spawn a delayed drain.
        let tx2 = tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let _ = rx.recv().await; // drain one item
        });

        let result = tx2.send_timeout(3, Duration::from_millis(200)).await;
        assert!(result.is_ok());
    }

    /// `send_timeout` returns `Err` on genuine timeout.
    #[tokio::test]
    async fn channel_send_timeout_fails() {
        let (tx, _rx) = bounded::<u32>(1);
        tx.try_send(1).unwrap();
        let result = tx.send_timeout(2, Duration::from_millis(10)).await;
        assert!(result.is_err());
    }

    // -- router tests --------------------------------------------------------

    /// Register + duplicate kick.
    #[tokio::test]
    async fn duplicate_kick() {
        let router = Router::new();
        let (tx1, _rx1) = bounded::<Bytes>(8);
        let (tx2, _rx2) = bounded::<Bytes>(8);

        router.register("room1", "alice", tx1);
        router.register("room1", "alice", tx2); // kicks tx1

        let room = router.rooms.get("room1").unwrap();
        assert_eq!(room.len(), 1);
    }

    /// Unicast to missing user → silent drop (no panic).
    #[tokio::test]
    async fn unicast_missing_user() {
        let router = Router::new();
        let (tx, _rx) = bounded::<Bytes>(8);
        router.register("room1", "bob", tx);

        let header = PacketHeader::new(
            PacketType::AiEvent,
            TargetType::Unicast,
            "room1",
            "nobody",
            1,
            0,
        )
        .unwrap();
        let payload = Bytes::new();
        router.route(header, payload).await;
    }

    /// Broadcast to missing room → silent drop (no panic).
    #[tokio::test]
    async fn broadcast_missing_room() {
        let router = Router::new();
        let header = PacketHeader::new(
            PacketType::AiEvent,
            TargetType::Broadcast,
            "ghost",
            "alice",
            1,
            0,
        )
        .unwrap();
        let payload = Bytes::new();
        router.route(header, payload).await;
    }

    /// Room cleanup on last user leaving.
    #[tokio::test]
    async fn room_cleanup() {
        let router = Router::new();
        let (tx, _rx) = bounded::<Bytes>(8);
        router.register("room1", "alice", tx);
        assert!(router.rooms.contains_key("room1"));
        router.unregister("room1", "alice");
        assert!(!router.rooms.contains_key("room1"));
    }

    /// Broadcast delivers to all users in the room.
    #[tokio::test]
    async fn broadcast_delivers_to_all() {
        let router = Router::new();
        // Two users in the same room.
        let (tx1, mut rx1) = bounded::<Bytes>(8);
        let (tx2, mut rx2) = bounded::<Bytes>(8);
        router.register("room1", "alice", tx1);
        router.register("room1", "bob", tx2);

        let payload = Bytes::from_static(b"hello");
        let header = PacketHeader::new(
            PacketType::AiEvent,
            TargetType::Broadcast,
            "room1",
            "sender",
            1,
            payload.len(),
        )
        .unwrap();

        router.route(header, payload.clone()).await;

        // Both users should receive the payload.
        assert_eq!(rx1.recv().await, Some(payload.clone()));
        assert_eq!(rx2.recv().await, Some(payload));
    }

    /// Unicast delivers only to the targeted user.
    #[tokio::test]
    async fn unicast_delivers_to_one() {
        let router = Router::new();
        let (tx1, mut rx1) = bounded::<Bytes>(8);
        let (tx2, mut rx2) = bounded::<Bytes>(8);
        router.register("room1", "alice", tx1);
        router.register("room1", "bob", tx2);

        let payload = Bytes::from_static(b"secret");
        let header = PacketHeader::new(
            PacketType::AiEvent,
            TargetType::Unicast,
            "room1",
            "alice",
            1,
            payload.len(),
        )
        .unwrap();

        router.route(header, payload.clone()).await;

        // Alice gets it, Bob does not.
        assert_eq!(rx1.recv().await, Some(payload));
        assert!(rx2.try_recv().is_err()); // still empty
    }

    /// RawMotion uses send_drop_oldest (eviction on full channel).
    #[tokio::test]
    async fn raw_motion_drops_oldest() {
        let router = Router::new();
        let (tx, mut rx) = bounded::<Bytes>(2); // tiny capacity
        router.register("room1", "alice", tx);

        // Fill the channel with two frames.
        let p1 = Bytes::from_static(b"frame1");
        let h1 = PacketHeader::new(
            PacketType::RawMotion,
            TargetType::Unicast,
            "room1",
            "alice",
            1,
            p1.len(),
        )
        .unwrap();
        router.route(h1, p1).await;

        let p2 = Bytes::from_static(b"frame2");
        let h2 = PacketHeader::new(
            PacketType::RawMotion,
            TargetType::Unicast,
            "room1",
            "alice",
            2,
            p2.len(),
        )
        .unwrap();
        router.route(h2, p2).await;

        // Third frame should evict the oldest (frame1).
        let p3 = Bytes::from_static(b"frame3");
        let h3 = PacketHeader::new(
            PacketType::RawMotion,
            TargetType::Unicast,
            "room1",
            "alice",
            3,
            p3.len(),
        )
        .unwrap();
        router.route(h3, p3).await;

        // frame1 is gone, frame2 and frame3 remain.
        assert_eq!(rx.recv().await, Some(Bytes::from_static(b"frame2")));
        assert_eq!(rx.recv().await, Some(Bytes::from_static(b"frame3")));
    }

    // -- helper for BoundedReceiver ------------------------------------------

    impl<T> BoundedReceiver<T> {
        /// Non-blocking receive attempt. Returns `Err(())` when empty.
        #[cfg(test)]
        fn try_recv(&mut self) -> Result<T, ()> {
            let mut q = self.inner.queue.lock().unwrap();
            q.pop_front().ok_or(())
        }
    }
}
