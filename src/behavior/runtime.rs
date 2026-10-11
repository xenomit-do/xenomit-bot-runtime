use super::*;

pub(super) fn publish(report: &Mutex<Option<String>>, line: impl Into<String>) {
    if let Ok(mut slot) = report.lock() {
        *slot = Some(line.into());
    }
}

impl BehaviorRuntime for PlannerRuntime {
    fn tick_stopped(
        &self,
        client: Arc<dyn GameClient>,
        config: &ModuleConfig,
        snapshot: Option<&GameSnapshot>,
    ) -> Result<(), String> {
        // Hold the worker gate so a stopped tick cannot race Start or an active worker.
        let worker = self.worker.lock().map_err(|_| "Worker lock was poisoned")?;
        if worker
            .as_ref()
            .is_some_and(|w| w.running.load(Ordering::Acquire))
        {
            return Ok(());
        }
        let settings = config
            .settings
            .get("behaviors")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let selection = plugin_selection(config)?;
        let (_, selected_behaviors) = self.registries_for(&selection)?;
        self.validate_rule_services(&settings, &selected_behaviors)?;
        let mut session = self
            .behavior_session
            .lock()
            .map_err(|_| "Behavior session lock was poisoned")?;
        if session
            .as_ref()
            .is_none_or(|current| current.selection != selection)
        {
            let rules = if self.plugin_catalog.is_some() {
                selected_behaviors
            } else {
                (*self.behavior_registry).clone()
            };
            *session = Some(RuleSession {
                runner: Arc::new(Mutex::new(rules.create(&settings)?)),
                started: Instant::now(),
                selection: selection.clone(),
            });
        }
        let session = session.as_mut().expect("initialized behavior session");
        let mut runner = session
            .runner
            .lock()
            .map_err(|_| "Behavior lock was poisoned")?;
        runner.update(&settings)?;
        let normalized = snapshot.cloned().map(|mut s| {
            normalize_world_positions(&mut s);
            apply_global_target_constraints(&mut s, &config.settings);
            s
        });
        let now_ms = session
            .started
            .elapsed()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64;
        let output = runner.tick(&RuleContext {
            now_ms,
            snapshot: normalized.as_ref(),
            module_settings: &config.settings,
            stopped: true,
        });
        // Safe stop is explicitly requested by the toolbar and must also reach
        // the gate exit when the objective is already paused.
        if settings["stop_button"]["enabled"].as_bool() == Some(true)
            && settings["stop_button"]["options"]["requested"].as_bool() == Some(true)
            && let (Some(plan), Some(snapshot)) = (&output.takeover, &normalized)
        {
            let mut executor = ActionExecutor {
                now_ms,
                ..Default::default()
            };
            execute_decision(&*client, snapshot, plan, &mut executor, &config.settings)?;
        }
        let commands = output.commands.into_iter().chain(
            output
                .takeover
                .into_iter()
                .flat_map(|p| p.controls.commands),
        );
        for command in commands {
            client.game_command(&command).map_err(|e| e.to_string())?;
            runner.command_succeeded(&command, now_ms);
        }
        for action in output.host_actions {
            if let HostAction::SelectTarget(id) = action {
                if let Some(snapshot) = &normalized {
                    select_rule_target(&*client, snapshot, id)?;
                }
            } else if action != HostAction::RequestRefresh {
                self.host_actions.handle(action);
            }
        }
        if !output.status.is_empty() {
            publish(&self.report, output.status.join("; "));
        }
        Ok(())
    }
    fn start(&self, client: Arc<dyn GameClient>, module: ModuleConfig) -> Result<(), String> {
        let module_settings = module.settings.clone();
        let behavior_settings = module
            .settings
            .get("behaviors")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let selection = plugin_selection(&module)?;
        let (selected_modules, selected_behaviors) = self.registries_for(&selection)?;
        let rule_registry = if self.plugin_catalog.is_some() {
            &selected_behaviors
        } else {
            &self.behavior_registry
        };
        let behaviors = Arc::new(Mutex::new(rule_registry.create(&behavior_settings)?));
        let rules_started = Instant::now();
        for (rule, service) in [
            ("urgent_detector", "game.urgent_quest"),
            ("calendar_bonus", "backpage.calendar"),
        ] {
            if behavior_settings
                .get(rule)
                .and_then(|v| v.get("enabled"))
                .and_then(Value::as_bool)
                == Some(true)
                && rule_registry
                    .descriptors()
                    .iter()
                    .any(|descriptor| descriptor.id == rule)
                && (rule != "urgent_detector"
                    || behavior_settings[rule]["options"]["take_quest"].as_bool() == Some(true))
                && !self.services.contains(service)
            {
                return Err(format!(
                    "Behavior {rule} requires service {service}, unavailable on this controller"
                ));
            }
        }
        let tick = module
            .settings
            .get("tick_ms")
            .and_then(Value::as_u64)
            .map(|ms| Duration::from_millis(ms.clamp(10, 60_000)))
            .unwrap_or(TICK);
        let mut slot = self
            .worker
            .lock()
            .map_err(|_| "behavior runtime lock was poisoned".to_owned())?;
        if slot
            .as_ref()
            .is_some_and(|worker| worker.running.load(Ordering::Acquire))
        {
            return Err("a behavior module is already running".into());
        }
        if let Some(previous) = slot.take() {
            let _ = previous.thread.join();
        }

        let stop = CancellationToken::default();
        let module_registry = if self.plugin_catalog.is_some() {
            &selected_modules
        } else {
            &self.registry
        };
        let runner = module_registry.create(module, stop.clone())?;
        *self
            .behavior_session
            .lock()
            .map_err(|_| "Behavior session lock was poisoned")? = Some(RuleSession {
            runner: Arc::clone(&behaviors),
            started: rules_started,
            selection: selection.clone(),
        });
        for service in &runner.descriptor().required_services {
            if service != "game.call"
                && service != "game.command"
                && !self.services.contains(service)
            {
                return Err(format!("Required service {service} is unavailable"));
            }
        }
        let services = self.services.clone().with_game_client(client.clone());
        let running = Arc::new(AtomicBool::new(true));
        let worker_stop = stop.clone();
        let worker_running = Arc::clone(&running);
        let report_sink = Arc::clone(&self.report);
        let pending = Arc::clone(&self.pending);
        let host_actions = Arc::clone(&self.host_actions);
        let handoff = self.handoff.clone();
        if let Ok(mut slot) = self.pending.lock() {
            *slot = None;
        }
        if let Ok(mut report) = self.report.lock() {
            *report = Some("Module started; waiting for the first game snapshot".into());
        }
        let thread = thread::Builder::new()
            .name("xenomit-behavior".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_loop(
                        client,
                        runner,
                        services,
                        worker_stop,
                        tick,
                        Arc::clone(&report_sink),
                        pending,
                        behaviors,
                        module_settings,
                        behavior_settings,
                        rules_started,
                        host_actions,
                        handoff,
                    )
                }))
                .unwrap_or_else(|_| Err("behavior worker panicked".into()));
                worker_running.store(false, Ordering::Release);
                if let Err(error) = &result {
                    publish(&report_sink, format!("Behavior module exited: {error}"));
                }
                result
            })
            .map_err(|error| format!("could not start behavior worker: {error}"))?;
        *slot = Some(Worker {
            stop,
            running,
            thread,
            selection,
        });
        Ok(())
    }

    fn stop(&self) -> Result<(), String> {
        let mut slot = self
            .worker
            .lock()
            .map_err(|_| "behavior runtime lock was poisoned".to_owned())?;
        let Some(worker) = slot.take() else {
            return Ok(());
        };
        worker.stop.cancel();
        worker.thread.thread().unpark();
        let result = worker
            .thread
            .join()
            .map_err(|_| "behavior worker panicked".to_owned())?;
        if let Ok(mut report) = self.report.lock() {
            *report = None;
        }
        result
    }

    fn report(&self) -> Option<String> {
        self.report.lock().ok().and_then(|line| line.clone())
    }

    fn modules(&self) -> Result<Vec<xenomit_game_api::ModuleDescriptor>, String> {
        if let Some(catalog) = &self.plugin_catalog {
            let enabled = catalog
                .descriptors()
                .into_iter()
                .map(|plugin| (plugin.id, true))
                .collect();
            self.modules_for(&enabled)
        } else {
            Ok(self.registry.descriptors())
        }
    }

    fn plugin_descriptors(&self) -> Result<Vec<xenomit_game_api::PluginDescriptor>, String> {
        Ok(self
            .plugin_catalog
            .as_ref()
            .map(|catalog| catalog.descriptors())
            .unwrap_or_default())
    }

    fn plugin_diagnostics(&self) -> Vec<String> {
        self.plugin_diagnostics.clone()
    }

    fn modules_for(
        &self,
        enabled: &PluginSelection,
    ) -> Result<Vec<xenomit_game_api::ModuleDescriptor>, String> {
        let Some(catalog) = &self.plugin_catalog else {
            return self.modules();
        };
        let mut modules = catalog.module_descriptors(enabled)?;
        let behaviors = catalog.behavior_settings_descriptor(enabled)?;
        let mut plugins = catalog.settings_descriptor();
        if !self.plugin_diagnostics.is_empty() {
            plugins.description.push_str("\n\nManifest diagnostics:\n");
            plugins
                .description
                .push_str(&self.plugin_diagnostics.join("\n"));
        }
        modules.push(behaviors);
        modules.push(plugins);
        Ok(modules)
    }

    fn is_running(&self) -> bool {
        self.worker.lock().ok().is_some_and(|slot| {
            slot.as_ref()
                .is_some_and(|worker| worker.running.load(Ordering::Acquire))
        })
    }

    fn update(&self, module: &ModuleConfig) {
        let selection = match plugin_selection(module) {
            Ok(selection) => selection,
            Err(error) => {
                publish(&self.report, error);
                return;
            }
        };
        if self.plugin_catalog.is_some()
            && self
                .worker
                .lock()
                .ok()
                .and_then(|slot| slot.as_ref().map(|worker| worker.selection != selection))
                == Some(true)
        {
            publish(
                &self.report,
                "Plugin settings cannot change while the behavior is starting or running; stop first.",
            );
            return;
        }
        if let Ok(mut slot) = self.pending.lock() {
            *slot = Some(module.clone());
        }
    }
}

pub(super) fn plugin_selection(config: &ModuleConfig) -> Result<PluginSelection, String> {
    let Some(value) = config.settings.get("plugins") else {
        return Ok(PluginSelection::default());
    };
    let plugins = value
        .as_object()
        .ok_or("Plugin settings must be an object of plugin IDs to booleans")?;
    plugins
        .iter()
        .map(|(id, value)| {
            value
                .as_bool()
                .map(|enabled| (id.clone(), enabled))
                .ok_or_else(|| format!("Plugin {id} enabled setting must be a boolean"))
        })
        .collect()
}
