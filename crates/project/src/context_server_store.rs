pub mod extension;
pub mod registry;

use std::{path::Path, sync::Arc};

use anyhow::{anyhow, Context as _, Result};
use crate::worktree_store::WorktreeStore;
use gpui::{App, AsyncApp}; // Ensured App and AsyncApp are here
use log::warn;
use collections::{HashMap, HashSet}; // FxHashMap might be collections::FxHashMap
use context_server::{ContextServer, ContextServerCommand, ContextServerId};
use gpui::{Context, Entity, EventEmitter, Subscription, Task, WeakEntity, actions}; // Removed App, AsyncApp from here as they are imported above
use registry::ContextServerDescriptorRegistry;
use settings::{Settings as _, SettingsStore, WorktreeId}; // Added WorktreeId
use util::ResultExt as _;
use crate::{
    project_settings::{ContextServerConfiguration, ProjectSettings},
};


// Helper function to substitute $ZED_WORKTREE_ROOT in a command
fn substitute_command_placeholders(
    command: &mut ContextServerCommand,
    worktree_root_path: &str,
    server_id_for_log: &ContextServerId,
) -> bool { // Returns true if all placeholders were substituted or no placeholders were present
    let mut all_substituted_or_not_present = true;
    log::warn!(
        "$ZED_WORKTREE_ROOT: Substituting for server_id '{}'. Path: {}",
        server_id_for_log,
        worktree_root_path
    );

    let placeholder = "$ZED_WORKTREE_ROOT";

    // Substitute in command path
    if command.path.contains(placeholder) {
        log::warn!("$ZED_WORKTREE_ROOT: Replacing in path: '{}'", command.path);
        command.path = command.path.replace(placeholder, worktree_root_path);
        if command.path.contains(placeholder) { // Check if substitution was successful
            log::error!("$ZED_WORKTREE_ROOT: Failed to substitute in path for server_id '{}'. Path remains: {}", server_id_for_log, command.path);
            all_substituted_or_not_present = false;
        }
    }

    // Substitute in command arguments
    for arg in command.args.iter_mut() {
        if arg.contains(placeholder) {
            log::warn!("$ZED_WORKTREE_ROOT: Replacing in arg: '{}'", arg);
            *arg = arg.replace(placeholder, worktree_root_path);
            if arg.contains(placeholder) { // Check if substitution was successful
                log::error!("$ZED_WORKTREE_ROOT: Failed to substitute in arg '{}' for server_id '{}'. Arg remains: {}", arg, server_id_for_log, arg);
                all_substituted_or_not_present = false;
            }
        }
    }

    // Substitute in environment variables
    // Assuming ContextServerCommand.env is Option<std::collections::HashMap<String, String>>
    if let Some(env_map) = command.env.as_mut() {
        for val in env_map.values_mut() {
            if val.contains(placeholder) {
                log::warn!("$ZED_WORKTREE_ROOT: Replacing in env var value: '{}'", val);
                *val = val.replace(placeholder, worktree_root_path);
                if val.contains(placeholder) { // Check if substitution was successful
                    log::error!("$ZED_WORKTREE_ROOT: Failed to substitute in env value '{}' for server_id '{}'. Value remains: {}", val, server_id_for_log, val);
                    all_substituted_or_not_present = false;
                }
            }
        }
    }
    all_substituted_or_not_present
}


pub fn init(cx: &mut App) {
    extension::init(cx);
}

actions!(context_server, [Restart]);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ContextServerStatus {
    Starting,
    Running,
    Stopped,
    Error(Arc<str>),
}

impl ContextServerStatus {
    fn from_state(state: &ContextServerState) -> Self {
        match state {
            ContextServerState::Starting { .. } => ContextServerStatus::Starting,
            ContextServerState::Running { .. } => ContextServerStatus::Running,
            ContextServerState::Stopped { error, .. } => {
                if let Some(error) = error {
                    ContextServerStatus::Error(error.clone())
                } else {
                    ContextServerStatus::Stopped
                }
            }
        }
    }
}

enum ContextServerState {
    Starting {
        server: Arc<ContextServer>,
        configuration: Arc<ContextServerConfiguration>,
        _task: Task<()>,
    },
    Running {
        server: Arc<ContextServer>,
        configuration: Arc<ContextServerConfiguration>,
    },
    Stopped {
        server: Arc<ContextServer>,
        configuration: Arc<ContextServerConfiguration>,
        error: Option<Arc<str>>,
    },
}

impl ContextServerState {
    pub fn server(&self) -> Arc<ContextServer> {
        match self {
            ContextServerState::Starting { server, .. } => server.clone(),
            ContextServerState::Running { server, .. } => server.clone(),
            ContextServerState::Stopped { server, .. } => server.clone(),
        }
    }

    pub fn configuration(&self) -> Arc<ContextServerConfiguration> {
        match self {
            ContextServerState::Starting { configuration, .. } => configuration.clone(),
            ContextServerState::Running { configuration, .. } => configuration.clone(),
            ContextServerState::Stopped { configuration, .. } => configuration.clone(),
        }
    }
}

pub type ContextServerFactory =
    Box<dyn Fn(ContextServerId, Arc<ContextServerConfiguration>) -> Arc<ContextServer>>;

pub struct ContextServerStore {
    servers: HashMap<ContextServerId, ContextServerState>,
    worktree_store: Entity<WorktreeStore>,
    registry: Entity<ContextServerDescriptorRegistry>,
    update_servers_task: Option<Task<Result<()>>>,
    context_server_factory: Option<ContextServerFactory>,
    needs_server_update: bool,
    _subscriptions: Vec<Subscription>,
}

pub enum Event {
    ServerStatusChanged {
        server_id: ContextServerId,
        status: ContextServerStatus,
    },
}

impl EventEmitter<Event> for ContextServerStore {}

impl ContextServerStore {
    pub fn new(worktree_store: Entity<WorktreeStore>, cx: &mut Context<Self>) -> Self {
        Self::new_internal(
            true,
            None,
            ContextServerDescriptorRegistry::default_global(cx),
            worktree_store,
            cx,
        )
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn test(
        registry: Entity<ContextServerDescriptorRegistry>,
        worktree_store: Entity<WorktreeStore>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_internal(false, None, registry, worktree_store, cx)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn test_maintain_server_loop(
        context_server_factory: ContextServerFactory,
        registry: Entity<ContextServerDescriptorRegistry>,
        worktree_store: Entity<WorktreeStore>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_internal(
            true,
            Some(context_server_factory),
            registry,
            worktree_store,
            cx,
        )
    }

    fn new_internal(
        maintain_server_loop: bool,
        context_server_factory: Option<ContextServerFactory>,
        registry: Entity<ContextServerDescriptorRegistry>,
        worktree_store: Entity<WorktreeStore>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscriptions = if maintain_server_loop {
            vec![
                cx.observe(&registry, |this, _registry, cx| {
                    this.available_context_servers_changed(cx);
                }),
                cx.observe_global::<SettingsStore>(|this, cx| {
                    this.available_context_servers_changed(cx);
                }),
            ]
        } else {
            Vec::new()
        };

        let mut this = Self {
            _subscriptions: subscriptions,
            worktree_store,
            registry,
            needs_server_update: false,
            servers: HashMap::default(),
            update_servers_task: None,
            context_server_factory,
        };
        if maintain_server_loop {
            this.available_context_servers_changed(cx);
        }
        this
    }

    pub fn get_server(&self, id: &ContextServerId) -> Option<Arc<ContextServer>> {
        self.servers.get(id).map(|state| state.server())
    }

    pub fn get_running_server(&self, id: &ContextServerId) -> Option<Arc<ContextServer>> {
        if let Some(ContextServerState::Running { server, .. }) = self.servers.get(id) {
            Some(server.clone())
        } else {
            None
        }
    }

    pub fn status_for_server(&self, id: &ContextServerId) -> Option<ContextServerStatus> {
        self.servers.get(id).map(ContextServerStatus::from_state)
    }

    pub fn all_server_ids(&self) -> Vec<ContextServerId> {
        self.servers.keys().cloned().collect()
    }

    pub fn running_servers(&self) -> Vec<Arc<ContextServer>> {
        self.servers
            .values()
            .filter_map(|state| {
                if let ContextServerState::Running { server, .. } = state {
                    Some(server.clone())
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn start_server(
        &mut self,
        server: Arc<ContextServer>,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        let location = self
            .worktree_store
            .read(cx)
            .visible_worktrees(cx)
            .next()
            .map(|worktree| settings::SettingsLocation {
                worktree_id: worktree.read(cx).id(),
                path: Path::new(""),
            });
        let settings = ProjectSettings::get(location, cx);
        let configuration = settings
            .context_servers
            .get(&server.id().0)
            .context("Failed to load context server configuration from settings")?
            .clone();

        self.run_server(server, Arc::new(configuration), cx);
        Ok(())
    }

    pub fn stop_server(&mut self, id: &ContextServerId, cx: &mut Context<Self>) -> Result<()> {
        let state = self
            .servers
            .remove(id)
            .context("Context server not found")?;

        let server = state.server();
        let configuration = state.configuration();
        let mut result = Ok(());
        if let ContextServerState::Running { server, .. } = &state {
            result = server.stop();
        }
        drop(state);

        self.update_server_state(
            id.clone(),
            ContextServerState::Stopped {
                configuration,
                server,
                error: None,
            },
            cx,
        );

        result
    }

    pub fn restart_server(&mut self, id: &ContextServerId, cx: &mut Context<Self>) -> Result<()> {
        if let Some(state) = self.servers.get(&id) {
            let configuration = state.configuration();

            self.stop_server(&state.server().id(), cx)?;
            // Pass &*cx which dereferences &mut Context<Self> to &App, then to &AppContext
            // Pass None for source_worktree_id as restart context might not match original source context
            let new_server = self.create_context_server(id.clone(), configuration.clone(), &*cx, None)?;
            self.run_server(new_server, configuration, cx);
        }
        Ok(())
    }

    fn run_server(
        &mut self,
        server: Arc<ContextServer>,
        configuration: Arc<ContextServerConfiguration>,
        cx: &mut Context<Self>,
    ) {
        let id = server.id();
        if matches!(
            self.servers.get(&id),
            Some(ContextServerState::Starting { .. } | ContextServerState::Running { .. })
        ) {
            self.stop_server(&id, cx).log_err();
        }

        let task = cx.spawn({
            let id = server.id();
            let server = server.clone();
            let configuration = configuration.clone();
            async move |this, cx| {
                match server.clone().start(&cx).await {
                    Ok(_) => {
                        log::info!("Started {} context server", id);
                        debug_assert!(server.client().is_some());

                        this.update(cx, |this, cx| {
                            this.update_server_state(
                                id.clone(),
                                ContextServerState::Running {
                                    server,
                                    configuration,
                                },
                                cx,
                            )
                        })
                        .log_err()
                    }
                    Err(err) => {
                        log::error!("{} context server failed to start: {}", id, err);
                        this.update(cx, |this, cx| {
                            this.update_server_state(
                                id.clone(),
                                ContextServerState::Stopped {
                                    configuration,
                                    server,
                                    error: Some(err.to_string().into()),
                                },
                                cx,
                            )
                        })
                        .log_err()
                    }
                };
            }
        });

        self.update_server_state(
            id.clone(),
            ContextServerState::Starting {
                configuration,
                _task: task,
                server,
            },
            cx,
        );
    }

    fn remove_server(&mut self, id: &ContextServerId, cx: &mut Context<Self>) -> Result<()> {
        let state = self
            .servers
            .remove(id)
            .context("Context server not found")?;
        drop(state);
        cx.emit(Event::ServerStatusChanged {
            server_id: id.clone(),
            status: ContextServerStatus::Stopped,
        });
        Ok(())
    }

    fn is_configuration_valid(&self, configuration: &ContextServerConfiguration) -> bool {
        // Command must be some when we are running in stdio mode.
        self.context_server_factory.as_ref().is_some() || configuration.command.is_some()
    }

    fn create_context_server(
        &self,
        id: ContextServerId,
        configuration: Arc<ContextServerConfiguration>,
        app_cx: &App,
        source_worktree_id: Option<WorktreeId>, // NEW: To distinguish global vs local config source
    ) -> Result<Arc<ContextServer>> {
        warn!("ContextServerStore: create_context_server for id: {}, source_worktree_id: {:?}", id, source_worktree_id);

        let mut command_to_run = match configuration.command.clone() {
            Some(c) => c,
            None => {
                return Err(anyhow!(
                    "ContextServerId {}: Missing command in configuration",
                    id
                ))
            }
        };

        // --- Логика подстановки плейсхолдера ---
        let mut command_is_valid_for_launch = true;
        let placeholder = "$ZED_WORKTREE_ROOT";
        let placeholder_is_used = command_to_run.path.contains(placeholder)
            || command_to_run
                .args
                .iter()
                .any(|arg| arg.contains(placeholder))
            || command_to_run.env.as_ref().map_or(false, |em| {
                em.values().any(|v| v.contains(placeholder))
            });

        if placeholder_is_used {
            let mut worktree_path_for_substitution: Option<String> = None;
            let mut substitution_attempted = false;

            if let Some(wt_id) = source_worktree_id { // Config is from a specific project
                substitution_attempted = true;
                warn!("ContextServerStore: Server id '{}' uses placeholder and is from project settings (worktree_id: {:?}). Attempting to resolve path.", id, wt_id);
                worktree_path_for_substitution = self.worktree_store.read(app_cx)
                    .worktree_for_id(wt_id, app_cx)
                    .and_then(|wh_entity| {
                        wh_entity.read(app_cx).as_local().map(|lw| {
                            let path_str = lw.abs_path().to_string_lossy().into_owned();
                            warn!("ContextServerStore: Resolved path for worktree_id {:?}: {}", wt_id, path_str);
                            path_str
                        })
                    });
                if worktree_path_for_substitution.is_none() {
                    warn!("ContextServerStore: Could not resolve path for specific worktree_id {:?}. It might not be a local worktree or not found in store.", wt_id);
                }
            } else { // Config is global
                warn!("ContextServerStore: Server id '{}' uses placeholder in global config. Placeholder will not be substituted.", id);
                // For global configs, we don't substitute $ZED_WORKTREE_ROOT.
                // command_is_valid_for_launch remains true, server will launch with placeholder.
                // If this should be an error, set command_is_valid_for_launch = false here.
            }

            if substitution_attempted { // Only if we tried to substitute (i.e., for local config)
                if let Some(ref actual_worktree_path) = worktree_path_for_substitution {
                    if !substitute_command_placeholders(
                        &mut command_to_run,
                        actual_worktree_path,
                        &id,
                    ) {
                        // substitute_command_placeholders уже залогировала ошибку
                        command_is_valid_for_launch = false;
                    }
                } else { // Path for substitution was needed (local config) but not resolved
                    warn!(
                        "ContextServerStore: Server_id '{}': {} is used with project settings, but its path could not be resolved. Server cannot be created with this command.",
                        id, placeholder
                    );
                    command_is_valid_for_launch = false;
                }
            }
            // If !substitution_attempted (i.e. global config), command_is_valid_for_launch is still true by default.
        }


        if !command_is_valid_for_launch {
            return Err(anyhow!(
                "ContextServerId {}: Failed to resolve {} for command. Check logs for details.",
                id,
                placeholder
            ));
        }
        // --- Конец логики подстановки ---

        if let Some(factory) = self.context_server_factory.as_ref() {
            // Если фабрика используется, она должна либо принимать измененную команду,
            // либо мы не можем здесь выполнить подстановку для фабричных серверов, если они сами читают команду из config.
            // Передача оригинальной configuration может быть проблемой, если фабрика использует configuration.command.
            warn!("ContextServerStore: Using factory for server_id {}. Placeholder substitution might not apply if factory re-reads command from config. Command after potential substitution (not passed to factory): {:?}", id, command_to_run);
            Ok(factory(id, configuration))
        } else {
            Ok(Arc::new(ContextServer::stdio(id, command_to_run))) // Используем измененную command_to_run
        }
    }

    fn update_server_state(
        &mut self,
        id: ContextServerId,
        state: ContextServerState,
        cx: &mut Context<Self>,
    ) {
        let status = ContextServerStatus::from_state(&state);
        self.servers.insert(id.clone(), state);
        cx.emit(Event::ServerStatusChanged {
            server_id: id,
            status,
        });
    }

    fn available_context_servers_changed(&mut self, cx: &mut Context<Self>) {
        if self.update_servers_task.is_some() {
            self.needs_server_update = true;
        } else {
            self.needs_server_update = false;
            self.update_servers_task = Some(cx.spawn(async move |this, cx| {
                if let Err(err) = Self::maintain_servers(this.clone(), cx).await {
                    log::error!("Error maintaining context servers: {}", err);
                }

                this.update(cx, |this, cx| {
                    this.update_servers_task.take();
                    if this.needs_server_update {
                        this.available_context_servers_changed(cx);
                    }
                })?;

                Ok(())
            }));
        }
    }

    async fn maintain_servers(this: WeakEntity<Self>, cx: &mut AsyncApp) -> Result<()> {
        let mut desired_servers = collections::FxHashMap::default();

        let (registry, worktree_store, source_worktree_id_for_settings) = this.update(cx, |this, cx| {
            let location = this
                .worktree_store
                .read(cx)
                .visible_worktrees(cx)
                .next()
                .map(|worktree| settings::SettingsLocation {
                    worktree_id: worktree.read(cx).id(),
                    path: Path::new(""),
                });
            // let settings = ProjectSettings::get(location, cx); // Original line, one below is clone
            // Convert from settings.context_servers (HashMap<Arc<str>, ...>)
            // to desired_servers (FxHashMap<ContextServerId, ...>)
            let settings = ProjectSettings::get(location, cx); // Moved original line here, and location.clone() is correct
            let servers_from_settings: &std::collections::HashMap<Arc<str>, ContextServerConfiguration> = &settings.context_servers;
            desired_servers = servers_from_settings.iter().map(|(k, v)| {
                (ContextServerId(k.clone()), v.clone())
            }).collect::<collections::FxHashMap<ContextServerId, ContextServerConfiguration>>();

            let source_worktree_id_for_these_settings = location.map(|loc| loc.worktree_id);

            (this.registry.clone(), this.worktree_store.clone(), source_worktree_id_for_these_settings) // Pass it out
        })?;


        for (id, descriptor) in
            registry.read_with(cx, |registry, _| registry.context_server_descriptors())?
        {
            // Now desired_servers is FxHashMap<ContextServerId, ContextServerConfiguration>
            // and id from the loop is Arc<str>.
            let config = desired_servers.entry(ContextServerId(id.clone())).or_default();
            if config.command.is_none() {
                if let Some(extension_command) = descriptor
                    .command(worktree_store.clone(), &cx)
                    .await
                    .log_err()
                {
                    config.command = Some(extension_command);
                }
            }
        }

        this.update(cx, |this, _| {
            // Filter out configurations without commands, the user uninstalled an extension.
            desired_servers.retain(|_, configuration| this.is_configuration_valid(configuration));
        })?;

        let mut servers_to_start = Vec::new();
        let mut servers_to_remove = HashSet::default();
        let mut servers_to_stop = HashSet::default();

        this.update(cx, |this, _cx| {
            for server_id in this.servers.keys() {
                // All servers that are not in desired_servers should be removed from the store.
                // E.g. this can happen if the user removed a server from the configuration,
                // or the user uninstalled an extension.
                // server_id here is &ContextServerId from this.servers.keys()
                if !desired_servers.contains_key(server_id) {
                    servers_to_remove.insert(server_id.clone());
                }
            }

            for (id, config) in desired_servers {
                // id from this loop is already ContextServerId because desired_servers is FxHashMap<ContextServerId, _>
                // The line `let id = ContextServerId(id.clone());` was removed as it's incorrect.

                let existing_config = this.servers.get(&id).map(|state| state.configuration());
                if existing_config.as_deref() != Some(&config) {
                    let config = Arc::new(config);
                    if let Some(server) = this
                        .create_context_server(id.clone(), config.clone(), &*_cx, source_worktree_id_for_settings) // Pass AppContext and source_worktree_id
                        .log_err()
                    {
                        servers_to_start.push((server, config));
                        if this.servers.contains_key(&id) {
                            servers_to_stop.insert(id);
                        }
                    }
                }
            }
        })?;

        for id in servers_to_stop {
            this.update(cx, |this, cx| this.stop_server(&id, cx).ok())?;
        }

        for id in servers_to_remove {
            this.update(cx, |this, cx| this.remove_server(&id, cx).ok())?;
        }

        for (server, config) in servers_to_start {
            this.update(cx, |this, cx| this.run_server(server, config, cx))
                .log_err();
        }

        Ok(())
    }
}
