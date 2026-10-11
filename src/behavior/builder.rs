use super::*;

impl PlannerRuntime {
    pub(super) fn validate_rule_services(
        &self,
        settings: &Value,
        registry: &BehaviorRegistry,
    ) -> Result<(), String> {
        for descriptor in registry.descriptors() {
            if settings[&descriptor.id]["enabled"].as_bool() == Some(true) {
                for service in descriptor.required_services {
                    if service != "game.call"
                        && service != "game.command"
                        && !self.services.contains(&service)
                    {
                        return Err(format!(
                            "Behavior {} requires service {service}, unavailable on this controller",
                            descriptor.id
                        ));
                    }
                }
            }
        }
        let active: std::collections::BTreeSet<_> = registry
            .descriptors()
            .into_iter()
            .map(|rule| rule.id)
            .collect();
        for (rule, service) in [
            ("urgent_detector", "game.urgent_quest"),
            ("calendar_bonus", "backpage.calendar"),
        ] {
            let enabled =
                active.contains(rule) && settings[rule]["enabled"].as_bool() == Some(true);
            let needs_service = rule != "urgent_detector"
                || settings[rule]["options"]["take_quest"].as_bool() == Some(true);
            if enabled && needs_service && !self.services.contains(service) {
                return Err(format!(
                    "Behavior {rule} requires service {service}, unavailable on this controller"
                ));
            }
        }
        Ok(())
    }
    pub fn with_registry(registry: ModuleRegistry, services: ServiceRegistry) -> Self {
        Self::with_behavior_registry(registry, builtin_behavior_registry(), services)
    }
    pub fn with_behavior_registry(
        registry: ModuleRegistry,
        behaviors: BehaviorRegistry,
        services: ServiceRegistry,
    ) -> Self {
        Self {
            registry: Arc::new(registry),
            behavior_registry: Arc::new(behaviors),
            plugin_catalog: None,
            plugin_diagnostics: Vec::new(),
            behavior_session: Mutex::new(None),
            services,
            host_actions: Arc::new(HostActionQueue),
            worker: Mutex::new(None),
            report: Arc::new(Mutex::new(None)),
            pending: Arc::new(Mutex::new(None)),
            handoff: None,
        }
    }

    /// Install a custom [`HostActionHandler`] (e.g. a headless front-end
    /// performing re-login/exit in-process). The default is
    /// [`HostActionQueue`], preserving the GUI's poll-driven dispatch.
    pub fn with_host_action_handler(mut self, handler: Arc<dyn HostActionHandler>) -> Self {
        self.host_actions = handler;
        self
    }

    /// Install the shared drain/checkpoint channel (headless daemons only).
    /// The same `Arc` is held by the daemon's control loop.
    pub fn with_handoff_link(mut self, link: Arc<HandoffLink>) -> Self {
        self.handoff = Some(link);
        self
    }
    /// The installed [`HandoffLink`], if any.
    pub fn handoff(&self) -> Option<Arc<HandoffLink>> {
        self.handoff.clone()
    }

    pub fn with_catalog(
        catalog: PluginCatalog,
        diagnostics: Vec<String>,
        services: ServiceRegistry,
    ) -> Self {
        let mut diagnostics = diagnostics;
        let selection = catalog
            .descriptors()
            .into_iter()
            .map(|plugin| (plugin.id, true))
            .collect();
        let (registry, behaviors) = match catalog.registries(&selection) {
            Ok(registries) => registries,
            Err(error) => {
                diagnostics.push(format!("Plugin activation catalogue: {error}"));
                let fallback = PluginCatalog::builtin();
                let fallback_selection = fallback
                    .descriptors()
                    .into_iter()
                    .map(|plugin| (plugin.id, true))
                    .collect();
                fallback
                    .registries(&fallback_selection)
                    .expect("built-in plugin selection")
            }
        };
        Self {
            registry: Arc::new(registry),
            behavior_registry: Arc::new(behaviors),
            plugin_catalog: Some(Arc::new(catalog)),
            plugin_diagnostics: diagnostics,
            behavior_session: Mutex::new(None),
            services,
            host_actions: Arc::new(HostActionQueue),
            worker: Mutex::new(None),
            report: Arc::new(Mutex::new(None)),
            pending: Arc::new(Mutex::new(None)),
            handoff: None,
        }
    }

    pub(super) fn registries_for(
        &self,
        selection: &PluginSelection,
    ) -> Result<(ModuleRegistry, BehaviorRegistry), String> {
        if let Some(catalog) = &self.plugin_catalog {
            catalog.registries(selection)
        } else {
            Ok(((*self.registry).clone(), (*self.behavior_registry).clone()))
        }
    }
}
