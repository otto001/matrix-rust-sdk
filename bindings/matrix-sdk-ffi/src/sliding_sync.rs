use std::sync::{Arc, RwLock};

use anyhow::Context;
use eyeball_im::VectorDiff;
use futures_util::{future::join4, pin_mut, StreamExt};
use matrix_sdk::ruma::{
    api::client::sync::sync_events::{
        v4::RoomSubscription as RumaRoomSubscription,
        UnreadNotificationsCount as RumaUnreadNotificationsCount,
    },
    assign, IdParseError, OwnedRoomId, RoomId, UInt,
};
pub use matrix_sdk::{
    room::timeline::Timeline, ruma::api::client::sync::sync_events::v4::SyncRequestListFilters,
    Client as MatrixClient, LoopCtrl, RoomListEntry as MatrixRoomEntry,
    SlidingSyncBuilder as MatrixSlidingSyncBuilder, SlidingSyncMode, SlidingSyncState,
};
use tokio::task::JoinHandle;
use tracing::{debug, error, warn};
use url::Url;

use crate::{
    helpers::unwrap_or_clone_arc, room::TimelineLock, Client, EventTimelineItem, Room,
    TimelineDiff, TimelineItem, TimelineListener, RUNTIME,
};

type TaskHandleFinalizer = Box<dyn FnOnce() + Send + Sync>;

pub struct TaskHandle {
    handle: JoinHandle<()>,
    finalizer: RwLock<Option<TaskHandleFinalizer>>,
}

impl TaskHandle {
    // Create a new task handle.
    fn new(handle: JoinHandle<()>) -> Self {
        Self { handle, finalizer: RwLock::new(None) }
    }

    /// Define a function that will run after the handle has been aborted.
    fn set_finalizer(&mut self, finalizer: TaskHandleFinalizer) {
        *self.finalizer.write().unwrap() = Some(finalizer);
    }
}

#[uniffi::export]
impl TaskHandle {
    pub fn cancel(&self) {
        debug!("stoppable.cancel() called");

        self.handle.abort();

        if let Some(finalizer) = self.finalizer.write().unwrap().take() {
            finalizer();
        }
    }

    /// Check whether the handle is finished.
    pub fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }
}

impl Drop for TaskHandle {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[derive(uniffi::Object)]
pub struct UnreadNotificationsCount {
    highlight_count: u32,
    notification_count: u32,
}

#[uniffi::export]
impl UnreadNotificationsCount {
    pub fn highlight_count(&self) -> u32 {
        self.highlight_count
    }
    pub fn notification_count(&self) -> u32 {
        self.notification_count
    }
    pub fn has_notifications(&self) -> bool {
        self.notification_count != 0 || self.highlight_count != 0
    }
}

impl From<RumaUnreadNotificationsCount> for UnreadNotificationsCount {
    fn from(inner: RumaUnreadNotificationsCount) -> Self {
        UnreadNotificationsCount {
            highlight_count: inner
                .highlight_count
                .and_then(|x| x.try_into().ok())
                .unwrap_or_default(),
            notification_count: inner
                .notification_count
                .and_then(|x| x.try_into().ok())
                .unwrap_or_default(),
        }
    }
}

#[derive(uniffi::Error)]
pub enum SlidingSyncError {
    /// The response we've received from the server can't be parsed or doesn't
    /// match up with the current expectations on the client side. A
    /// `sync`-restart might be required.
    BadResponse {
        msg: String,
    },
    /// Called `.build()` on a builder type, but the given required field was
    /// missing.
    BuildMissingField {
        msg: String,
    },
    /// A `SlidingSyncListRequestGenerator` has been used without having been
    /// initialized. It happens when a response is handled before a request has
    /// been sent. It usually happens when testing.
    RequestGeneratorHasNotBeenInitialized {
        msg: String,
    },
    /// Someone has tried to modify a sliding sync list's ranges, but the
    /// selected sync mode doesn't allow that.
    CannotModifyRanges {
        msg: String,
    },
    /// Ranges have a `start` bound greater than `end`.
    InvalidRange {
        /// Start bound.
        start: u32,
        /// End bound.
        end: u32,
    },
    Unknown {
        error: String,
    },
}

impl From<matrix_sdk::sliding_sync::Error> for SlidingSyncError {
    fn from(value: matrix_sdk::sliding_sync::Error) -> Self {
        use matrix_sdk::sliding_sync::Error as E;

        match value {
            E::BadResponse(msg) => Self::BadResponse { msg },
            E::BuildMissingField(msg) => Self::BuildMissingField { msg: msg.to_owned() },
            E::RequestGeneratorHasNotBeenInitialized(msg) => {
                Self::RequestGeneratorHasNotBeenInitialized { msg }
            }
            E::CannotModifyRanges(msg) => Self::CannotModifyRanges { msg },
            E::InvalidRange { start, end } => Self::InvalidRange { start, end },
            error => Self::Unknown { error: error.to_string() },
        }
    }
}

pub struct SlidingSyncRoom {
    inner: matrix_sdk::SlidingSyncRoom,
    timeline: TimelineLock,
    runner: matrix_sdk::SlidingSync,
    client: Client,
}

#[uniffi::export]
impl SlidingSyncRoom {
    pub fn name(&self) -> Option<String> {
        self.inner.name().map(ToOwned::to_owned)
    }

    pub fn room_id(&self) -> String {
        self.inner.room_id().to_string()
    }

    pub fn is_dm(&self) -> Option<bool> {
        self.inner.is_dm()
    }

    pub fn is_initial(&self) -> Option<bool> {
        self.inner.is_initial_response()
    }

    pub fn is_loading_more(&self) -> bool {
        self.inner.is_loading_more()
    }

    pub fn has_unread_notifications(&self) -> bool {
        self.inner.has_unread_notifications()
    }

    pub fn unread_notifications(&self) -> Arc<UnreadNotificationsCount> {
        Arc::new(self.inner.unread_notifications().clone().into())
    }

    pub fn full_room(&self) -> Option<Arc<Room>> {
        self.client
            .get_room(self.inner.room_id())
            .map(|room| Arc::new(Room::with_timeline(room, self.timeline.clone())))
    }

    pub fn avatar_url(&self) -> Option<String> {
        Some(self.client.get_room(self.inner.room_id())?.avatar_url()?.into())
    }

    #[allow(clippy::significant_drop_in_scrutinee)]
    pub fn latest_room_message(&self) -> Option<Arc<EventTimelineItem>> {
        let item = RUNTIME.block_on(self.inner.latest_event())?;
        Some(Arc::new(EventTimelineItem(item)))
    }
}

impl SlidingSyncRoom {
    pub fn add_timeline_listener(
        &self,
        listener: Box<dyn TimelineListener>,
    ) -> anyhow::Result<SlidingSyncSubscribeResult> {
        let (items, stoppable_spawn) = self.add_timeline_listener_inner(listener)?;

        Ok(SlidingSyncSubscribeResult { items, task_handle: Arc::new(stoppable_spawn) })
    }

    pub fn subscribe_and_add_timeline_listener(
        &self,
        listener: Box<dyn TimelineListener>,
        settings: Option<RoomSubscription>,
    ) -> anyhow::Result<SlidingSyncSubscribeResult> {
        let (items, mut stoppable_spawn) = self.add_timeline_listener_inner(listener)?;
        let room_id = self.inner.room_id().clone();

        self.runner.subscribe(room_id.clone(), settings.map(Into::into));

        let runner = self.runner.clone();
        stoppable_spawn.set_finalizer(Box::new(move || runner.unsubscribe(room_id)));

        Ok(SlidingSyncSubscribeResult { items, task_handle: Arc::new(stoppable_spawn) })
    }

    fn add_timeline_listener_inner(
        &self,
        listener: Box<dyn TimelineListener>,
    ) -> anyhow::Result<(Vec<Arc<TimelineItem>>, TaskHandle)> {
        let mut timeline_lock = self.timeline.write().unwrap();
        let timeline = match &*timeline_lock {
            Some(timeline) => timeline,
            None => {
                let timeline = RUNTIME
                    .block_on(self.inner.timeline())
                    .context("Could not set timeline listener: room not found.")?;
                timeline_lock.insert(Arc::new(timeline))
            }
        };

        #[allow(unknown_lints, clippy::redundant_async_block)] // false positive
        let (timeline_items, timeline_stream) = RUNTIME.block_on(timeline.subscribe());

        let handle_events = async move {
            let listener: Arc<dyn TimelineListener> = listener.into();
            timeline_stream
                .for_each(move |diff| {
                    let listener = listener.clone();
                    let fut = RUNTIME.spawn_blocking(move || {
                        listener.on_update(Arc::new(TimelineDiff::new(diff)))
                    });

                    async move {
                        if let Err(e) = fut.await {
                            error!("Timeline listener error: {e}");
                        }
                    }
                })
                .await;
        };

        let handle_sliding_sync_reset = {
            let reset_broadcast_rx = self.client.sliding_sync_reset_broadcast_tx.subscribe();
            let timeline = timeline.to_owned();
            async move {
                reset_broadcast_rx.for_each(|_| timeline.clear()).await;
            }
        };

        let handle_sync_gap = {
            let gap_broadcast_rx = self.client.client.subscribe_sync_gap(self.inner.room_id());
            let timeline = timeline.to_owned();
            async move {
                gap_broadcast_rx.for_each(|_| timeline.clear()).await;
            }
        };

        // This in the future could be removed, and the rx handling could be moved
        // inside handle_sliding_sync_reset since we want to reset the sliding
        // sync for ignore user list events
        let handle_ignore_user_list_changes = {
            let ignore_user_list_change_rx = self.client.subscribe_to_ignore_user_list_changes();
            let timeline = timeline.to_owned();
            async move {
                ignore_user_list_change_rx.for_each(|_| timeline.clear()).await;
            }
        };

        let items = timeline_items.into_iter().map(TimelineItem::from_arc).collect();
        let task_handle = TaskHandle::new(RUNTIME.spawn(async move {
            join4(
                handle_events,
                handle_sliding_sync_reset,
                handle_sync_gap,
                handle_ignore_user_list_changes,
            )
            .await;
        }));

        Ok((items, task_handle))
    }
}

pub struct SlidingSyncSubscribeResult {
    pub items: Vec<Arc<TimelineItem>>,
    pub task_handle: Arc<TaskHandle>,
}

pub struct UpdateSummary {
    /// The lists (according to their name), which have seen an update
    pub lists: Vec<String>,
    pub rooms: Vec<String>,
}

pub struct RequiredState {
    pub key: String,
    pub value: String,
}

pub struct RoomSubscription {
    pub required_state: Option<Vec<RequiredState>>,
    pub timeline_limit: Option<u32>,
}

impl From<RoomSubscription> for RumaRoomSubscription {
    fn from(val: RoomSubscription) -> Self {
        assign!(RumaRoomSubscription::default(), {
            required_state: val.required_state.map(|r|
                r.into_iter().map(|s| (s.key.into(), s.value)).collect()
            ).unwrap_or_default(),
            timeline_limit: val.timeline_limit.map(|u| u.into())
        })
    }
}

impl From<matrix_sdk::UpdateSummary> for UpdateSummary {
    fn from(other: matrix_sdk::UpdateSummary) -> Self {
        Self {
            lists: other.lists,
            rooms: other.rooms.into_iter().map(|r| r.as_str().to_owned()).collect(),
        }
    }
}

pub enum SlidingSyncListRoomsListDiff {
    Append { values: Vec<RoomListEntry> },
    Insert { index: u32, value: RoomListEntry },
    Set { index: u32, value: RoomListEntry },
    Remove { index: u32 },
    PushBack { value: RoomListEntry },
    PushFront { value: RoomListEntry },
    PopBack,
    PopFront,
    Clear,
    Reset { values: Vec<RoomListEntry> },
}

impl From<VectorDiff<MatrixRoomEntry>> for SlidingSyncListRoomsListDiff {
    fn from(other: VectorDiff<MatrixRoomEntry>) -> Self {
        match other {
            VectorDiff::Append { values } => {
                Self::Append { values: values.into_iter().map(|e| (&e).into()).collect() }
            }
            VectorDiff::Insert { index, value } => {
                Self::Insert { index: index as u32, value: (&value).into() }
            }
            VectorDiff::Set { index, value } => {
                Self::Set { index: index as u32, value: (&value).into() }
            }
            VectorDiff::Remove { index } => Self::Remove { index: index as u32 },
            VectorDiff::PushBack { value } => Self::PushBack { value: (&value).into() },
            VectorDiff::PushFront { value } => Self::PushFront { value: (&value).into() },
            VectorDiff::PopBack => Self::PopBack,
            VectorDiff::PopFront => Self::PopFront,
            VectorDiff::Clear => Self::Clear,
            VectorDiff::Reset { values } => {
                warn!("Room list subscriber lagged behind and was reset");
                Self::Reset { values: values.into_iter().map(|e| (&e).into()).collect() }
            }
        }
    }
}

#[derive(Clone, Debug)]
pub enum RoomListEntry {
    Empty,
    Invalidated { room_id: String },
    Filled { room_id: String },
}

impl From<&MatrixRoomEntry> for RoomListEntry {
    fn from(other: &MatrixRoomEntry) -> Self {
        match other {
            MatrixRoomEntry::Empty => Self::Empty,
            MatrixRoomEntry::Filled(b) => Self::Filled { room_id: b.to_string() },
            MatrixRoomEntry::Invalidated(b) => Self::Invalidated { room_id: b.to_string() },
        }
    }
}

pub trait SlidingSyncListRoomItemsObserver: Sync + Send {
    fn did_receive_update(&self);
}

pub trait SlidingSyncListRoomListObserver: Sync + Send {
    fn did_receive_update(&self, diff: SlidingSyncListRoomsListDiff);
}

pub trait SlidingSyncListRoomsCountObserver: Sync + Send {
    fn did_receive_update(&self, new_count: u32);
}

pub trait SlidingSyncListStateObserver: Sync + Send {
    fn did_receive_update(&self, new_state: SlidingSyncState);
}

#[derive(Clone)]
pub struct SlidingSyncListBuilder {
    inner: matrix_sdk::SlidingSyncListBuilder,
}

#[derive(uniffi::Record)]
pub struct SlidingSyncRequestListFilters {
    pub is_dm: Option<bool>,
    pub spaces: Vec<String>,
    pub is_encrypted: Option<bool>,
    pub is_invite: Option<bool>,
    pub is_tombstoned: Option<bool>,
    pub room_types: Vec<String>,
    pub not_room_types: Vec<String>,
    pub room_name_like: Option<String>,
    pub tags: Vec<String>,
    pub not_tags: Vec<String>,
    // pub extensions: BTreeMap<String, Value>,
}

impl From<SlidingSyncRequestListFilters> for SyncRequestListFilters {
    fn from(value: SlidingSyncRequestListFilters) -> Self {
        let SlidingSyncRequestListFilters {
            is_dm,
            spaces,
            is_encrypted,
            is_invite,
            is_tombstoned,
            room_types,
            not_room_types,
            room_name_like,
            tags,
            not_tags,
        } = value;

        assign!(SyncRequestListFilters::default(), {
            is_dm, spaces, is_encrypted, is_invite, is_tombstoned, room_types, not_room_types, room_name_like, tags, not_tags,
        })
    }
}

impl SlidingSyncListBuilder {
    pub fn new() -> Self {
        Self { inner: matrix_sdk::SlidingSyncList::builder() }
    }

    pub fn sync_mode(self: Arc<Self>, mode: SlidingSyncMode) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.inner = builder.inner.sync_mode(mode);
        Arc::new(builder)
    }

    pub fn ranges(self: Arc<Self>, ranges: Vec<(u32, u32)>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.inner = builder.inner.ranges(ranges);
        Arc::new(builder)
    }

    pub fn build(self: Arc<Self>) -> anyhow::Result<Arc<SlidingSyncList>> {
        let builder = unwrap_or_clone_arc(self);
        Ok(Arc::new(builder.inner.build()?.into()))
    }
}

#[uniffi::export]
impl SlidingSyncListBuilder {
    pub fn sort(self: Arc<Self>, sort: Vec<String>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.inner = builder.inner.sort(sort);
        Arc::new(builder)
    }

    pub fn required_state(self: Arc<Self>, required_state: Vec<RequiredState>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.inner = builder
            .inner
            .required_state(required_state.into_iter().map(|s| (s.key.into(), s.value)).collect());
        Arc::new(builder)
    }

    pub fn filters(self: Arc<Self>, filters: SlidingSyncRequestListFilters) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.inner = builder.inner.filters(Some(filters.into()));
        Arc::new(builder)
    }

    pub fn no_filters(self: Arc<Self>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.inner = builder.inner.filters(None);
        Arc::new(builder)
    }

    pub fn batch_size(self: Arc<Self>, batch_size: u32) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.inner = builder.inner.full_sync_batch_size(batch_size);
        Arc::new(builder)
    }

    pub fn room_limit(self: Arc<Self>, limit: u32) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.inner = builder.inner.full_sync_maximum_number_of_rooms_to_fetch(limit);
        Arc::new(builder)
    }

    pub fn no_room_limit(self: Arc<Self>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.inner = builder.inner.full_sync_maximum_number_of_rooms_to_fetch(None);
        Arc::new(builder)
    }

    pub fn timeline_limit(self: Arc<Self>, limit: u32) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.inner = builder.inner.timeline_limit(limit);
        Arc::new(builder)
    }

    pub fn no_timeline_limit(self: Arc<Self>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.inner = builder.inner.no_timeline_limit();
        Arc::new(builder)
    }

    pub fn name(self: Arc<Self>, name: String) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.inner = builder.inner.name(name);
        Arc::new(builder)
    }

    pub fn add_range(self: Arc<Self>, from: u32, to: u32) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.inner = builder.inner.add_range(from, to);
        Arc::new(builder)
    }

    pub fn reset_ranges(self: Arc<Self>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.inner = builder.inner.reset_ranges();
        Arc::new(builder)
    }
}

#[derive(Clone)]
pub struct SlidingSyncList {
    inner: matrix_sdk::SlidingSyncList,
}

impl From<matrix_sdk::SlidingSyncList> for SlidingSyncList {
    fn from(inner: matrix_sdk::SlidingSyncList) -> Self {
        SlidingSyncList { inner }
    }
}

impl SlidingSyncList {
    pub fn observe_state(
        &self,
        observer: Box<dyn SlidingSyncListStateObserver>,
    ) -> Arc<TaskHandle> {
        let mut state_stream = self.inner.state_stream();

        Arc::new(TaskHandle::new(RUNTIME.spawn(async move {
            loop {
                if let Some(new_state) = state_stream.next().await {
                    observer.did_receive_update(new_state);
                }
            }
        })))
    }

    pub fn observe_room_list(
        &self,
        observer: Box<dyn SlidingSyncListRoomListObserver>,
    ) -> Arc<TaskHandle> {
        let mut room_list_stream = self.inner.room_list_stream();

        Arc::new(TaskHandle::new(RUNTIME.spawn(async move {
            loop {
                if let Some(diff) = room_list_stream.next().await {
                    observer.did_receive_update(diff.into());
                }
            }
        })))
    }

    pub fn observe_rooms_count(
        &self,
        observer: Box<dyn SlidingSyncListRoomsCountObserver>,
    ) -> Arc<TaskHandle> {
        let mut rooms_count_stream = self.inner.maximum_number_of_rooms_stream();

        Arc::new(TaskHandle::new(RUNTIME.spawn(async move {
            loop {
                if let Some(Some(new)) = rooms_count_stream.next().await {
                    observer.did_receive_update(new);
                }
            }
        })))
    }
}

#[uniffi::export]
impl SlidingSyncList {
    /// Get the current list of rooms
    pub fn current_room_list(&self) -> Vec<RoomListEntry> {
        self.inner.room_list()
    }

    /// Reset the ranges to a particular set
    ///
    /// Remember to cancel the existing stream and fetch a new one as this will
    /// only be applied on the next request.
    pub fn set_range(&self, start: u32, end: u32) -> Result<(), SlidingSyncError> {
        self.inner.set_range(start, end).map_err(Into::into)
    }

    /// Set the ranges to fetch
    ///
    /// Remember to cancel the existing stream and fetch a new one as this will
    /// only be applied on the next request.
    pub fn add_range(&self, start: u32, end: u32) -> Result<(), SlidingSyncError> {
        self.inner.add_range((start, end)).map_err(Into::into)
    }

    /// Reset the ranges
    pub fn reset_ranges(&self) -> Result<(), SlidingSyncError> {
        self.inner.reset_ranges().map_err(Into::into)
    }

    /// Total of rooms matching the filter
    pub fn current_room_count(&self) -> Option<u32> {
        self.inner.maximum_number_of_rooms()
    }

    /// The current timeline limit
    pub fn get_timeline_limit(&self) -> Option<u32> {
        self.inner.timeline_limit().map(|limit| u32::try_from(limit).unwrap_or_default())
    }

    /// The current timeline limit
    pub fn set_timeline_limit(&self, value: u32) {
        self.inner.set_timeline_limit(Some(value))
    }

    /// Unset the current timeline limit
    pub fn unset_timeline_limit(&self) {
        self.inner.set_timeline_limit::<UInt>(None)
    }
}

pub trait SlidingSyncObserver: Sync + Send {
    fn did_receive_sync_update(&self, summary: UpdateSummary);
}

pub struct SlidingSync {
    inner: matrix_sdk::SlidingSync,
    client: Client,
    observer: Arc<RwLock<Option<Box<dyn SlidingSyncObserver>>>>,
}

impl SlidingSync {
    fn new(inner: matrix_sdk::SlidingSync, client: Client) -> Self {
        Self { inner, client, observer: Default::default() }
    }

    pub fn set_observer(&self, observer: Option<Box<dyn SlidingSyncObserver>>) {
        *self.observer.write().unwrap() = observer;
    }

    pub fn subscribe(
        &self,
        room_id: String,
        settings: Option<RoomSubscription>,
    ) -> anyhow::Result<()> {
        self.inner.subscribe(room_id.try_into()?, settings.map(Into::into));
        Ok(())
    }

    pub fn unsubscribe(&self, room_id: String) -> anyhow::Result<()> {
        self.inner.unsubscribe(room_id.try_into()?);
        Ok(())
    }

    pub fn get_room(&self, room_id: String) -> anyhow::Result<Option<Arc<SlidingSyncRoom>>> {
        let runner = self.inner.clone();

        Ok(self.inner.get_room(<&RoomId>::try_from(room_id.as_str())?).map(|inner| {
            Arc::new(SlidingSyncRoom {
                inner,
                runner,
                client: self.client.clone(),
                timeline: Default::default(),
            })
        }))
    }

    pub fn get_rooms(
        &self,
        room_ids: Vec<String>,
    ) -> anyhow::Result<Vec<Option<Arc<SlidingSyncRoom>>>> {
        let actual_ids = room_ids
            .into_iter()
            .map(OwnedRoomId::try_from)
            .collect::<Result<Vec<OwnedRoomId>, IdParseError>>()?;
        Ok(self
            .inner
            .get_rooms(actual_ids.into_iter())
            .into_iter()
            .map(|o| {
                o.map(|inner| {
                    Arc::new(SlidingSyncRoom {
                        inner,
                        runner: self.inner.clone(),
                        client: self.client.clone(),
                        timeline: Default::default(),
                    })
                })
            })
            .collect())
    }
}

#[uniffi::export]
impl SlidingSync {
    #[allow(clippy::significant_drop_in_scrutinee)]
    pub fn get_list(&self, name: String) -> Option<Arc<SlidingSyncList>> {
        self.inner.list(&name).map(|inner| Arc::new(SlidingSyncList { inner }))
    }

    pub fn add_list(&self, list: Arc<SlidingSyncList>) -> Option<Arc<SlidingSyncList>> {
        self.inner.add_list(list.inner.clone()).map(|inner| Arc::new(SlidingSyncList { inner }))
    }

    pub fn pop_list(&self, name: String) -> Option<Arc<SlidingSyncList>> {
        self.inner.pop_list(&name).map(|inner| Arc::new(SlidingSyncList { inner }))
    }

    pub fn add_common_extensions(&self) {
        self.inner.add_common_extensions();
    }

    pub fn reset_lists(&self) {
        self.inner.reset_lists()
    }

    pub fn sync(&self) -> Arc<TaskHandle> {
        let inner = self.inner.clone();
        let client = self.client.clone();
        let observer = self.observer.clone();

        Arc::new(TaskHandle::new(RUNTIME.spawn(async move {
            let stream = inner.stream();
            pin_mut!(stream);

            loop {
                let update_summary = match stream.next().await {
                    Some(Ok(update_summary)) => update_summary,

                    Some(Err(error)) => {
                        if client.process_sync_error(error) == LoopCtrl::Break {
                            warn!("loop was stopped by client error processing");
                            break;
                        } else {
                            continue;
                        }
                    }

                    None => {
                        warn!("Inner streaming loop ended unexpectedly");
                        break;
                    }
                };

                if let Some(ref observer) = *observer.read().unwrap() {
                    observer.did_receive_sync_update(update_summary.into());
                }
            }
        })))
    }
}

#[derive(Clone)]
pub struct SlidingSyncBuilder {
    inner: MatrixSlidingSyncBuilder,
    client: Client,
}

impl SlidingSyncBuilder {
    pub fn homeserver(self: Arc<Self>, url: String) -> anyhow::Result<Arc<Self>> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.inner = builder.inner.homeserver(url.parse()?);
        Ok(Arc::new(builder))
    }

    pub fn build(self: Arc<Self>) -> anyhow::Result<Arc<SlidingSync>> {
        let builder = unwrap_or_clone_arc(self);
        RUNTIME.block_on(async move {
            Ok(Arc::new(SlidingSync::new(builder.inner.build().await?, builder.client)))
        })
    }
}

#[uniffi::export]
impl SlidingSyncBuilder {
    pub fn storage_key(self: Arc<Self>, name: Option<String>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.inner = builder.inner.storage_key(name);
        Arc::new(builder)
    }

    pub fn add_list(self: Arc<Self>, v: Arc<SlidingSyncList>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        let list = unwrap_or_clone_arc(v);
        builder.inner = builder.inner.add_list(list.inner);
        Arc::new(builder)
    }

    pub fn with_common_extensions(self: Arc<Self>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.inner = builder.inner.with_common_extensions();
        Arc::new(builder)
    }

    pub fn without_e2ee_extension(self: Arc<Self>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.inner = builder.inner.without_e2ee_extension();
        Arc::new(builder)
    }

    pub fn without_to_device_extension(self: Arc<Self>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.inner = builder.inner.without_to_device_extension();
        Arc::new(builder)
    }

    pub fn without_account_data_extension(self: Arc<Self>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.inner = builder.inner.without_account_data_extension();
        Arc::new(builder)
    }

    pub fn without_receipt_extension(self: Arc<Self>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.inner = builder.inner.without_receipt_extension();
        Arc::new(builder)
    }

    pub fn without_typing_extension(self: Arc<Self>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.inner = builder.inner.without_typing_extension();
        Arc::new(builder)
    }

    pub fn with_all_extensions(self: Arc<Self>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.inner = builder.inner.with_all_extensions();
        Arc::new(builder)
    }
}

#[uniffi::export]
impl Client {
    pub fn sliding_sync(&self) -> Arc<SlidingSyncBuilder> {
        RUNTIME.block_on(async move {
            let mut inner = self.client.sliding_sync().await;
            if let Some(sliding_sync_proxy) = self
                .sliding_sync_proxy
                .read()
                .unwrap()
                .clone()
                .and_then(|p| Url::parse(p.as_str()).ok())
            {
                inner = inner.homeserver(sliding_sync_proxy);
            }
            Arc::new(SlidingSyncBuilder { inner, client: self.clone() })
        })
    }
}
