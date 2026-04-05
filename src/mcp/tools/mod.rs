use crate::mcp::dispatcher::Dispatcher;
use crate::mcp::push::PushRegistry;
use crate::mcp::queues::{HealthEventQueue, NotificationQueue, RequestAwaiterRegistry};
use crate::storage::Storage;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

pub mod manager;
pub mod worker;

pub fn register_manager_tools(
    dispatcher: &mut Dispatcher,
    storage: Arc<Storage>,
    push: Arc<RwLock<PushRegistry>>,
    notifications: Arc<Mutex<NotificationQueue>>,
    health_events: Arc<Mutex<HealthEventQueue>>,
    awaiters: Arc<Mutex<RequestAwaiterRegistry>>,
) {
    manager::register(dispatcher, storage, push, notifications, health_events, awaiters);
}

pub fn register_worker_tools(
    dispatcher: &mut Dispatcher,
    storage: Arc<Storage>,
    push: Arc<RwLock<PushRegistry>>,
    notifications: Arc<Mutex<NotificationQueue>>,
    health_events: Arc<Mutex<HealthEventQueue>>,
    awaiters: Arc<Mutex<RequestAwaiterRegistry>>,
) {
    worker::register(dispatcher, storage, push, notifications, health_events, awaiters);
}

impl Dispatcher {
    pub fn with_manager_tools(
        storage: Arc<Storage>,
        push: Arc<RwLock<PushRegistry>>,
        notifications: Arc<Mutex<NotificationQueue>>,
        health_events: Arc<Mutex<HealthEventQueue>>,
        awaiters: Arc<Mutex<RequestAwaiterRegistry>>,
    ) -> Self {
        let mut dispatcher = Self::new();
        register_manager_tools(
            &mut dispatcher,
            storage,
            push,
            notifications,
            health_events,
            awaiters,
        );
        dispatcher
    }

    pub fn with_all_tools(
        storage: Arc<Storage>,
        push: Arc<RwLock<PushRegistry>>,
        notifications: Arc<Mutex<NotificationQueue>>,
        health_events: Arc<Mutex<HealthEventQueue>>,
        awaiters: Arc<Mutex<RequestAwaiterRegistry>>,
    ) -> Self {
        let mut dispatcher = Self::new();
        register_manager_tools(
            &mut dispatcher,
            Arc::clone(&storage),
            Arc::clone(&push),
            Arc::clone(&notifications),
            Arc::clone(&health_events),
            Arc::clone(&awaiters),
        );
        register_worker_tools(
            &mut dispatcher,
            storage,
            push,
            notifications,
            health_events,
            awaiters,
        );
        dispatcher
    }

    pub fn for_daemon(
        storage: Arc<Storage>,
        push: Arc<RwLock<PushRegistry>>,
        notifications: Arc<Mutex<NotificationQueue>>,
        health_events: Arc<Mutex<HealthEventQueue>>,
        awaiters: Arc<Mutex<RequestAwaiterRegistry>>,
        launcher: Arc<crate::process::launcher::ProcessLauncher>,
    ) -> Self {
        let mut dispatcher = Self::new();
        manager::register_with_launcher(
            &mut dispatcher,
            Arc::clone(&storage),
            Arc::clone(&push),
            Arc::clone(&notifications),
            Arc::clone(&health_events),
            Arc::clone(&awaiters),
            launcher,
        );
        register_worker_tools(
            &mut dispatcher,
            storage,
            push,
            notifications,
            health_events,
            awaiters,
        );
        dispatcher
    }
}
