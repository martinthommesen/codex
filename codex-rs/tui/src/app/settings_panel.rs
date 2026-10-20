//! Persists `/settings` kill switches through the app server and mirrors saved values locally.

use super::config_persistence::features_from_effective_config;
use super::config_persistence::overridden_write_message;
use super::*;
use crate::config_update::format_config_error;
use crate::settings_panel::SettingsSwitch;
use crate::settings_panel::SwitchState;
use codex_protocol::config_types::WebSearchMode;

/// How a settings save ended, which decides where its notice can be shown.
#[derive(Debug, PartialEq, Eq)]
enum SaveOutcome {
    Saved,
    Failed,
}

impl App {
    pub(super) async fn save_settings_switch(
        &mut self,
        app_server: &mut AppServerSession,
        switch: SettingsSwitch,
        enabled: bool,
    ) {
        let label = switch.label();
        let remote_web_search =
            switch == SettingsSwitch::WebSearch && app_server.uses_remote_workspace();
        // Remote threads follow the app server's web search requirements, not this machine's.
        let remote_web_search_modes = if remote_web_search {
            match read_web_search_modes(app_server).await {
                Ok(modes) => Ok(modes),
                Err(err) => {
                    tracing::warn!(error = %err, "failed to read app server web search requirements");
                    Err(format!(
                        "could not read the app server's web search requirements: {}",
                        format_config_error(&err)
                    ))
                }
            }
        } else {
            Ok(None)
        };
        if remote_web_search && let Ok(modes) = &remote_web_search_modes {
            self.config.web_search_mode =
                web_search_constraint(modes.as_deref(), self.config.web_search_mode.value());
        }
        // Stage first so values pinned by requirements fail before anything is written.
        let mut next_config = self.config.clone();
        if remote_web_search {
            // This machine's config files do not decide the mode a remote server writes.
            next_config.config_layer_stack = without_user_layers(&next_config.config_layer_stack);
        }
        let edits = switch.edits(enabled, &next_config);
        if let Err(err) = switch.apply(&mut next_config, enabled) {
            tracing::warn!(error = %err, switch = label, "settings switch rejected by requirements");
            self.report_settings_save(
                format!("{label} is managed by your administrator: {err}"),
                SaveOutcome::Failed,
            );
            return;
        }
        let response = match crate::config_update::write_config_batch(
            app_server.request_handle(),
            edits,
        )
        .await
        {
            Ok(response) => response,
            Err(err) => {
                let error = format_config_error(&err);
                tracing::error!(error = %error, switch = label, "failed to save settings switch");
                self.report_settings_save(
                    format!("Could not save {label}: {error}"),
                    SaveOutcome::Failed,
                );
                return;
            }
        };
        let overridden = (response.status == WriteStatus::OkOverridden)
            .then(|| overridden_write_message(&response).to_string());
        // The server only checks user-level layers for overrides, so a trusted project, a profile,
        // this TUI's launch flags or a legacy alias in another layer can still win. Resolve what
        // new sessions will actually get.
        let SaveResolution { winner, unverified } = self
            .resolve_saved_settings(
                app_server,
                switch,
                enabled,
                &response.file_path,
                remote_web_search_modes.as_ref().map(|modes| modes.as_deref()).map_err(String::as_str),
            )
            .await
            .unwrap_or_else(|err| {
                tracing::warn!(error = %err, switch = label, "failed to resolve config after a settings save");
                SaveResolution {
                    winner: None,
                    unverified: Some(format_config_error(&err)),
                }
            });
        if unverified.is_some() && overridden.is_none() {
            switch.adopt(&mut self.config, &next_config);
        }
        let (notice, outcome) = save_notice(
            switch,
            enabled,
            switch.state(&self.config),
            switch.state(&next_config),
            SaveCheck {
                overridden,
                unverified,
                winner,
            },
        );
        self.chat_widget.apply_settings_switch(switch, &self.config);
        self.report_settings_save(notice, outcome);
    }

    /// Resolves the values new sessions will load and adopts this switch's effective state.
    async fn resolve_saved_settings(
        &mut self,
        app_server: &AppServerSession,
        switch: SettingsSwitch,
        enabled: bool,
        written: &AbsolutePathBuf,
        remote_web_search_modes: RemoteWebSearchModes<'_>,
    ) -> Result<SaveResolution> {
        if app_server.uses_remote_workspace() {
            // New remote sessions start where /new starts them, not in the displayed thread's cwd.
            let cwd = app_server.remote_cwd_override().unwrap_or(Path::new("."));
            let mut effective_config = crate::config_update::read_effective_config_with_layers(
                app_server.request_handle(),
                cwd.display().to_string(),
            )
            .await?;
            // The server cannot see the launch choices this TUI sends with each new thread.
            let launch_origins = self.config.config_layer_stack.origins();
            let launch = RemoteLaunch {
                overrides: crate::app_server_session::config_request_overrides_from_config(
                    &self.config,
                    app_server.thread_params_mode(),
                ),
                origins: &launch_origins,
                web_search_source: crate::app_server_session::forwarded_web_search_key(
                    &self.config,
                    app_server.thread_params_mode(),
                ),
                web_search_modes: remote_web_search_modes,
            };
            let resolution =
                resolve_remote_save(switch, enabled, &mut effective_config, &launch, written);
            adopt_effective_switch_state(&mut self.config, switch, &effective_config);
            return Ok(resolution);
        }

        // The app server shares this machine's files, so build what /new will load.
        let cwd = self.chat_widget.config_ref().cwd.to_path_buf();
        let rebuilt = self.rebuild_config_for_cwd(cwd).await?;
        switch.adopt(&mut self.config, &rebuilt);
        let layers = &rebuilt.config_layer_stack;
        let winner = switch_override_layer(
            switch,
            enabled,
            &serde_json::to_value(layers.effective_config()).unwrap_or_default(),
            layers.origins().iter().map(|(key, origin)| {
                (
                    key.as_str(),
                    origin.name.precedence(),
                    OverrideLayer::from_config(&origin.name, written),
                )
            }),
        );
        Ok(SaveResolution {
            winner,
            unverified: None,
        })
    }

    /// Shows the notice in the panel, or in the transcript when the panel was already closed.
    fn report_settings_save(&mut self, notice: String, outcome: SaveOutcome) {
        let statuses = crate::settings_panel::statuses(&self.config);
        if self
            .chat_widget
            .refresh_settings_panel(statuses, Some(notice.clone()))
        {
            return;
        }
        match outcome {
            SaveOutcome::Saved => self.chat_widget.add_info_message(notice, /*hint*/ None),
            SaveOutcome::Failed => self.chat_widget.add_error_message(notice),
        }
    }
}

/// What a save's check against new sessions found.
struct SaveCheck {
    /// The app server's own override message, when it reported one.
    overridden: Option<String>,
    /// Why new sessions could not be checked, when they could not.
    unverified: Option<String>,
    /// The layer and key that still win, when another layer overrides the save.
    winner: Option<(OverrideLayer, String)>,
}

/// The notice for a written save. `resolved` is the switch's state for new sessions and
/// `requested` the state the save staged.
fn save_notice(
    switch: SettingsSwitch,
    enabled: bool,
    resolved: SwitchState,
    requested: SwitchState,
    check: SaveCheck,
) -> (String, SaveOutcome) {
    let label = switch.label();
    if resolved == requested {
        if let Some(err) = check.unverified {
            return (
                format!(
                    "{label} saved, but /settings could not check what new sessions will get: {err}"
                ),
                SaveOutcome::Failed,
            );
        }
        return (
            crate::settings_panel::saved_notice(switch, enabled, resolved),
            SaveOutcome::Saved,
        );
    }
    // The local answer names the key; the app server's message is the fallback.
    let reason = match (check.winner, check.overridden) {
        (Some((layer, key)), _) => layer.describe(&key),
        (None, Some(message)) => message,
        (None, None) => "another config file still sets it".to_string(),
    };
    tracing::warn!(
        reason,
        switch = label,
        "settings switch save was overridden"
    );
    (
        format!("{label} saved, but it is overridden: {reason}"),
        SaveOutcome::Failed,
    )
}

/// Applies the launch choices this TUI sends with every new thread on top of what a remote app
/// server read, below the layers that outrank them. Returns the origin keys they now decide.
fn overlay_launch_overrides(
    effective_config: &mut ConfigReadResponse,
    launch_overrides: Option<&HashMap<String, serde_json::Value>>,
) -> HashSet<String> {
    let mut flagged = HashSet::new();
    let Some(launch_overrides) = launch_overrides else {
        return flagged;
    };
    let session_flags_precedence =
        codex_app_server_protocol::ConfigLayerSource::SessionFlags.precedence();
    let outranked =
        |key: &str, origins: &HashMap<String, codex_app_server_protocol::ConfigLayerMetadata>| {
            origins.iter().any(|(origin_key, origin)| {
                (origin_key == key || origin_key.starts_with(&format!("{key}.")))
                    && origin.name.precedence() > session_flags_precedence
            })
        };
    if let Some(mode) = launch_overrides.get("web_search")
        && !outranked("web_search", &effective_config.origins)
        && let Ok(mode) = serde_json::from_value::<WebSearchMode>(mode.clone())
    {
        effective_config.config.web_search = Some(mode);
        flagged.insert("web_search".to_string());
    }
    let Some(session_features) = launch_overrides
        .get("features")
        .and_then(serde_json::Value::as_object)
    else {
        return flagged;
    };
    let mut overlays = Vec::new();
    for (name, value) in session_features {
        let key = format!("features.{name}");
        if !outranked(&key, &effective_config.origins) {
            overlays.push((name.clone(), value.clone(), key));
        }
    }
    let features = effective_config
        .config
        .additional
        .entry("features".to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    let Some(features) = features.as_object_mut() else {
        return flagged;
    };
    for (name, value, key) in overlays {
        // Record leaf keys the way config origins do.
        let structured = codex_config::is_structured_feature_path(&["features", name.as_str()]);
        match &value {
            serde_json::Value::Object(table) => {
                flagged.extend(table.keys().map(|sub_key| format!("{key}.{sub_key}")));
            }
            serde_json::Value::Bool(_) if structured => {
                flagged.insert(format!("{key}.enabled"));
            }
            _ => {
                flagged.insert(key);
            }
        }
        match features.get_mut(&name) {
            Some(existing) => merge_feature_value(existing, value, structured),
            None => {
                features.insert(name, value);
            }
        }
    }
    flagged
}

/// Merges a launch value into a feature the way config layers merge: tables merge key by key,
/// and a structured feature such as `multi_agent_v2` keeps `enabled` across a bool and a table.
fn merge_feature_value(base: &mut serde_json::Value, overlay: serde_json::Value, structured: bool) {
    match (base, overlay) {
        (base @ serde_json::Value::Bool(_), serde_json::Value::Object(overlay)) if structured => {
            let mut table = serde_json::Map::from_iter([("enabled".to_string(), base.take())]);
            for (key, value) in overlay {
                merge_table_value(&mut table, key, value);
            }
            *base = serde_json::Value::Object(table);
        }
        (serde_json::Value::Object(table), serde_json::Value::Bool(enabled)) if structured => {
            table.insert("enabled".to_string(), serde_json::Value::Bool(enabled));
        }
        (serde_json::Value::Object(table), serde_json::Value::Object(overlay)) => {
            for (key, value) in overlay {
                merge_table_value(table, key, value);
            }
        }
        (base, overlay) => *base = overlay,
    }
}

fn merge_table_value(
    table: &mut serde_json::Map<String, serde_json::Value>,
    key: String,
    value: serde_json::Value,
) {
    match table.get_mut(&key) {
        Some(existing) => merge_feature_value(existing, value, /*structured*/ false),
        None => {
            table.insert(key, value);
        }
    }
}

/// The web search modes a remote app server's requirements accept, or `None` when they accept
/// every mode.
async fn read_web_search_modes(
    app_server: &AppServerSession,
) -> Result<Option<Vec<WebSearchMode>>> {
    let response: codex_app_server_protocol::ConfigRequirementsReadResponse = app_server
        .request_handle()
        .request_typed(
            codex_app_server_protocol::ClientRequest::ConfigRequirementsRead {
                request_id: codex_app_server_protocol::RequestId::String(format!(
                    "tui-settings-requirements-{}",
                    uuid::Uuid::new_v4()
                )),
                params: None,
            },
        )
        .await
        .wrap_err("configRequirements/read failed in TUI")?;
    Ok(response
        .requirements
        .and_then(|requirements| requirements.allowed_web_search_modes))
}

/// The mode core falls back to when requirements reject the configured one.
fn web_search_fallback(modes: &[WebSearchMode]) -> WebSearchMode {
    crate::settings_panel::ENABLED_WEB_SEARCH_MODES
        .into_iter()
        .find(|mode| modes.contains(mode))
        .unwrap_or(WebSearchMode::Disabled)
}

/// Mirrors how core constrains `web_search` to the modes requirements allow; disabled is always
/// allowed.
fn web_search_constraint(
    modes: Option<&[WebSearchMode]>,
    current: WebSearchMode,
) -> codex_config::Constrained<WebSearchMode> {
    let Some(modes) = modes else {
        return codex_config::Constrained::allow_any(current);
    };
    let allowed = modes.to_vec();
    let initial = if current == WebSearchMode::Disabled || allowed.contains(&current) {
        current
    } else {
        web_search_fallback(&allowed)
    };
    codex_config::Constrained::new(initial, move |mode| {
        if *mode == WebSearchMode::Disabled || allowed.contains(mode) {
            Ok(())
        } else {
            Err(codex_config::ConstraintError::InvalidValue {
                field_name: "web_search",
                candidate: mode.to_string(),
                allowed: format!("{allowed:?}"),
                requirement_source: codex_config::RequirementSource::Unknown,
            })
        }
    })
    .unwrap_or_else(|_| codex_config::Constrained::allow_any(initial))
}

/// Applies a remote app server's web search requirements to what it read, the way core falls
/// back. Returns whether they changed the mode.
fn constrain_web_search(
    effective_config: &mut ConfigReadResponse,
    modes: Option<&[WebSearchMode]>,
) -> bool {
    let (Some(modes), Some(mode)) = (modes, effective_config.config.web_search) else {
        return false;
    };
    if mode == WebSearchMode::Disabled || modes.contains(&mode) {
        return false;
    }
    effective_config.config.web_search = Some(web_search_fallback(modes));
    true
}

/// The launch choices this TUI sends with each new thread to a remote app server, and where they
/// came from.
type RemoteWebSearchModes<'a> = std::result::Result<Option<&'a [WebSearchMode]>, &'a str>;

struct RemoteLaunch<'a> {
    overrides: Option<HashMap<String, serde_json::Value>>,
    origins: &'a HashMap<String, codex_config::ConfigLayerMetadata>,
    /// The launch key behind a forwarded `web_search`.
    web_search_source: Option<String>,
    /// A failed requirements read must leave the save unverified, even if config/read succeeds.
    web_search_modes: RemoteWebSearchModes<'a>,
}

/// What a settings save resolves to for new sessions.
struct SaveResolution {
    winner: Option<(OverrideLayer, String)>,
    unverified: Option<String>,
}

/// Applies `launch` to what a remote app server read back and finds what still decides `switch`.
fn resolve_remote_save(
    switch: SettingsSwitch,
    enabled: bool,
    effective_config: &mut ConfigReadResponse,
    launch: &RemoteLaunch<'_>,
    written: &AbsolutePathBuf,
) -> SaveResolution {
    // thread/start replaces the app server's own `[features]` launch flags with the table this TUI
    // sends, so resolve what is left without them before adding this TUI's flags.
    let forwards_features = launch
        .overrides
        .as_ref()
        .is_some_and(|overrides| overrides.contains_key("features"));
    let mut unverified = (forwards_features && !drop_replaced_server_features(effective_config))
        .then(|| {
            "the app server did not report the config layers below its own feature flags"
                .to_string()
        });
    let flagged = overlay_launch_overrides(effective_config, launch.overrides.as_ref());
    let requirements_disallow = match launch.web_search_modes {
        Ok(modes) => constrain_web_search(effective_config, modes),
        Err(error) => {
            unverified.get_or_insert_with(|| error.to_string());
            false
        }
    };
    let flag_precedence = codex_app_server_protocol::ConfigLayerSource::SessionFlags.precedence();
    let web_search_source = launch.web_search_source.as_deref();
    let winner = switch_override_layer(
        switch,
        enabled,
        &serde_json::to_value(&effective_config.config).unwrap_or_default(),
        effective_config
            .origins
            .iter()
            .map(|(key, origin)| {
                let layer = OverrideLayer::from_api(&origin.name, written);
                (key.as_str(), origin.name.precedence(), layer)
            })
            .chain(flagged.iter().map(|key| {
                let source = launch_source_key(key, web_search_source);
                let layer = OverrideLayer::launch(launch.origins.get(source));
                (key.as_str(), flag_precedence, Some(layer))
            })),
    )
    .map(|(layer, key)| {
        // Name the launch key the user set, such as a legacy search feature.
        let launched = flagged.contains(&key)
            && matches!(layer, OverrideLayer::CommandLine | OverrideLayer::Profile);
        let key = if launched {
            launch_source_key(&key, web_search_source).to_string()
        } else {
            key
        };
        (layer, key)
    })
    .or_else(|| {
        requirements_disallow.then(|| (OverrideLayer::Requirements, "web_search".to_string()))
    });
    SaveResolution { winner, unverified }
}

/// Removes the app server's own `[features]` launch flags from what it read back and resolves
/// those keys from the layers below them, the way a thread this TUI starts sees them. Returns
/// false when the server reported its own flags but not its layers.
fn drop_replaced_server_features(effective_config: &mut ConfigReadResponse) -> bool {
    let replaced = effective_config
        .origins
        .iter()
        .filter(|(key, origin)| {
            key.starts_with("features.")
                && matches!(
                    origin.name,
                    codex_app_server_protocol::ConfigLayerSource::SessionFlags
                )
        })
        .map(|(key, _)| key.clone())
        .collect::<Vec<_>>();
    if replaced.is_empty() {
        return true;
    }
    let Some(layers) = effective_config.layers.as_ref() else {
        return false;
    };
    let features = effective_config
        .config
        .additional
        .entry("features".to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    for key in &replaced {
        // Layers come highest first, and no layer above the server's flags sets these keys.
        let fallback = layers
            .iter()
            .filter(|layer| {
                layer.disabled_reason.is_none()
                    && !matches!(
                        layer.name,
                        codex_app_server_protocol::ConfigLayerSource::SessionFlags
                    )
            })
            .find_map(|layer| value_at(&layer.config, key).map(|value| (layer, value.clone())));
        let path = key
            .strip_prefix("features.")
            .unwrap_or(key)
            .split('.')
            .collect::<Vec<_>>();
        match fallback {
            Some((layer, value)) => {
                set_feature_value(features, &path, value);
                effective_config.origins.insert(
                    key.clone(),
                    codex_app_server_protocol::ConfigLayerMetadata {
                        name: layer.name.clone(),
                        version: layer.version.clone(),
                    },
                );
            }
            None => {
                remove_feature_value(features, &path);
                effective_config.origins.remove(key.as_str());
            }
        }
    }
    // config/read reports some features as the server resolved them, its own flags and legacy
    // aliases included, so re-derive each feature a replaced flag set from the layer that set its
    // canonical key, or drop it when none did.
    let touched = replaced
        .iter()
        .filter_map(|key| {
            let name = key.strip_prefix("features.")?.split('.').next()?;
            codex_features::feature_for_key(name)
        })
        .filter(|feature| !codex_config::is_structured_feature_path(&["features", feature.key()]))
        .collect::<HashSet<_>>();
    for feature in touched {
        let key = format!("features.{}", feature.key());
        let set_by = effective_config.origins.get(&key).and_then(|origin| {
            layers
                .iter()
                .find(|layer| layer.name == origin.name)
                .and_then(|layer| value_at(&layer.config, &key))
        });
        match set_by {
            Some(value) => set_feature_value(features, &[feature.key()], value.clone()),
            None if !effective_config.origins.contains_key(&key) => {
                remove_feature_value(features, &[feature.key()]);
            }
            None => {}
        }
    }
    true
}

/// Sets a `[features]` value, keeping a structured feature's plain-bool form for `enabled`.
fn set_feature_value(features: &mut serde_json::Value, path: &[&str], value: serde_json::Value) {
    let Some(table) = features.as_object_mut() else {
        return;
    };
    match path {
        [] => {}
        [name] => {
            table.insert((*name).to_string(), value);
        }
        [name, "enabled"]
            if value.is_boolean()
                && !table.get(*name).is_some_and(serde_json::Value::is_object) =>
        {
            table.insert((*name).to_string(), value);
        }
        [name, rest @ ..] => {
            let entry = table
                .entry((*name).to_string())
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
            if !entry.is_object() {
                // A plain bool stands for `enabled`.
                let enabled = entry.take();
                *entry = serde_json::Value::Object(
                    enabled
                        .is_boolean()
                        .then(|| ("enabled".to_string(), enabled))
                        .into_iter()
                        .collect(),
                );
            }
            set_feature_value(entry, rest, value);
        }
    }
}

/// Removes a `[features]` value and any table it leaves empty.
fn remove_feature_value(features: &mut serde_json::Value, path: &[&str]) {
    let Some(table) = features.as_object_mut() else {
        return;
    };
    match path {
        [] => {}
        [name] => {
            table.remove(*name);
        }
        [name, rest @ ..] => {
            let Some(entry) = table.get_mut(*name) else {
                return;
            };
            if !entry.is_object() {
                // A plain bool stands for `enabled`.
                if rest == ["enabled"] {
                    table.remove(*name);
                }
                return;
            }
            remove_feature_value(entry, rest);
            if entry.as_object().is_some_and(serde_json::Map::is_empty) {
                table.remove(*name);
            }
        }
    }
}

/// The feature of `switch` that a `features.*` key sets, directly or through a legacy alias.
fn switch_feature_for_key(switch: SettingsSwitch, key: &str) -> Option<Feature> {
    let name = key.strip_prefix("features.")?;
    let name = name.strip_suffix(".enabled").unwrap_or(name);
    switch
        .features()
        .iter()
        .copied()
        .chain((switch == SettingsSwitch::Subagents).then_some(Feature::MultiAgentV2))
        .find(|feature| {
            std::iter::once(feature.key())
                .chain(codex_features::legacy_keys_for_feature(*feature))
                .any(|feature_key| feature_key == name)
        })
}

/// The launch key behind this TUI's forwarded value for `key`. A forwarded `web_search` can come
/// from a legacy search feature (`web_search_source`) instead of the key itself.
fn launch_source_key<'a>(key: &'a str, web_search_source: Option<&'a str>) -> &'a str {
    match web_search_source {
        Some(source) if key == "web_search" => source,
        _ => key,
    }
}

/// `stack` without this machine's user config files.
fn without_user_layers(stack: &codex_config::ConfigLayerStack) -> codex_config::ConfigLayerStack {
    let user_files = stack
        .layers_low_to_high()
        .filter_map(|layer| match &layer.name {
            codex_config::ConfigLayerSource::User { file, .. } => Some(file.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    user_files
        .iter()
        .try_fold(stack.clone(), |stack, file| {
            stack.with_user_config(file, toml::Value::Table(toml::Table::new()))
        })
        .unwrap_or_else(|_| stack.clone())
}

/// The kind of config layer that still decides a switch after a save.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OverrideLayer {
    CommandLine,
    /// A flag the remote app server was started with, which this TUI cannot change.
    ServerLaunch,
    Project,
    Profile,
    /// The main `config.toml` under a selected profile, whose file `/settings` writes instead.
    BaseUser,
    Managed,
    /// The app server's requirements, which reject the saved value.
    Requirements,
    Other,
}

impl OverrideLayer {
    /// `None` for the user config file `/settings` wrote.
    fn from_config(
        source: &codex_config::ConfigLayerSource,
        written: &AbsolutePathBuf,
    ) -> Option<Self> {
        use codex_config::ConfigLayerSource as Source;
        match source {
            Source::User { file, .. } if file == written => None,
            Source::User {
                profile: Some(_), ..
            } => Some(Self::Profile),
            Source::User { profile: None, .. } => Some(Self::BaseUser),
            Source::SessionFlags => Some(Self::CommandLine),
            Source::Project { .. } => Some(Self::Project),
            Source::Mdm { .. }
            | Source::EnterpriseManaged { .. }
            | Source::LegacyManagedConfigTomlFromFile { .. }
            | Source::LegacyManagedConfigTomlFromMdm => Some(Self::Managed),
            Source::PackagedDefaults { .. } | Source::System { .. } => Some(Self::Other),
        }
    }

    /// `None` for the user config file `/settings` wrote.
    fn from_api(
        source: &codex_app_server_protocol::ConfigLayerSource,
        written: &AbsolutePathBuf,
    ) -> Option<Self> {
        use codex_app_server_protocol::ConfigLayerSource as Source;
        match source {
            Source::User { file, .. } if file == written => None,
            Source::User {
                profile: Some(_), ..
            } => Some(Self::Profile),
            Source::User { profile: None, .. } => Some(Self::BaseUser),
            // config/read reports only the server's own flags; this TUI's come from `launch`.
            Source::SessionFlags => Some(Self::ServerLaunch),
            Source::Project { .. } => Some(Self::Project),
            Source::Mdm { .. }
            | Source::EnterpriseManaged { .. }
            | Source::LegacyManagedConfigTomlFromFile { .. }
            | Source::LegacyManagedConfigTomlFromMdm => Some(Self::Managed),
            Source::PackagedDefaults { .. } | Source::System { .. } => Some(Self::Other),
        }
    }

    /// Where a launch choice this TUI forwards to a remote app server came from.
    fn launch(origin: Option<&codex_config::ConfigLayerMetadata>) -> Self {
        match origin.map(|origin| &origin.name) {
            Some(codex_config::ConfigLayerSource::User {
                profile: Some(_), ..
            }) => Self::Profile,
            _ => Self::CommandLine,
        }
    }

    fn describe(self, key: &str) -> String {
        match self {
            Self::CommandLine => {
                format!("a command-line flag for this Codex session still sets `{key}`")
            }
            Self::ServerLaunch => {
                format!("the app server was started with a flag that still sets `{key}`")
            }
            Self::Project => format!("this project's .codex/config.toml still sets `{key}`"),
            Self::Profile => format!("the selected profile still sets `{key}`"),
            // `/settings` writes only the selected profile, so the user has to edit this file.
            Self::BaseUser => format!(
                "your main config.toml still sets `{key}`; remove it there, since /settings only writes the selected profile"
            ),
            Self::Managed => format!("a managed config still sets `{key}`"),
            Self::Requirements => {
                format!("the app server's requirements do not allow that `{key}` value")
            }
            Self::Other => format!("another config file still sets `{key}`"),
        }
    }
}

/// The highest-precedence layer, other than the file `/settings` wrote, whose resolved value still
/// keeps the switch from reaching `enabled`, with the key it sets.
fn switch_override_layer<'a>(
    switch: SettingsSwitch,
    enabled: bool,
    values: &serde_json::Value,
    origins: impl Iterator<Item = (&'a str, i16, Option<OverrideLayer>)>,
) -> Option<(OverrideLayer, String)> {
    origins
        .filter(|(key, ..)| {
            value_at(values, key).is_some_and(|value| opposes_save(switch, enabled, key, value))
        })
        .filter_map(|(key, precedence, layer)| Some((precedence, key, layer?)))
        // Ties break on the key so the notice does not depend on map order.
        .max_by(|(a_precedence, a_key, _), (b_precedence, b_key, _)| {
            a_precedence.cmp(b_precedence).then(b_key.cmp(a_key))
        })
        .map(|(_, key, layer)| (layer, key.to_string()))
}

/// Whether `key` resolving to `value` keeps the switch from reaching `enabled`.
fn opposes_save(
    switch: SettingsSwitch,
    enabled: bool,
    key: &str,
    value: &serde_json::Value,
) -> bool {
    decides_switch(switch, enabled, key)
        && match switch {
            SettingsSwitch::WebSearch => value
                .as_str()
                .is_some_and(|mode| (mode == "disabled") == enabled),
            SettingsSwitch::PluginsAndApps
            | SettingsSwitch::Skills
            | SettingsSwitch::Hooks
            | SettingsSwitch::AgentsMd
            | SettingsSwitch::Memories
            | SettingsSwitch::Subagents
            | SettingsSwitch::ImageGeneration
            | SettingsSwitch::Goals => value.as_bool().is_some_and(|value| value != enabled),
        }
}

/// Whether `key` can keep the switch from reaching `enabled`.
fn decides_switch(switch: SettingsSwitch, enabled: bool, key: &str) -> bool {
    match switch {
        SettingsSwitch::WebSearch => key == "web_search",
        // Subagents need `agents.enabled` and `multi_agent` to turn on, and `agents.enabled` and
        // `multi_agent_v2` off to turn off.
        SettingsSwitch::Subagents => {
            let deciding = if enabled {
                Feature::Collab
            } else {
                Feature::MultiAgentV2
            };
            key == "agents.enabled" || switch_feature_for_key(switch, key) == Some(deciding)
        }
        SettingsSwitch::PluginsAndApps
        | SettingsSwitch::Skills
        | SettingsSwitch::Hooks
        | SettingsSwitch::AgentsMd
        | SettingsSwitch::Memories
        | SettingsSwitch::ImageGeneration
        | SettingsSwitch::Goals => switch_feature_for_key(switch, key).is_some(),
    }
}

/// The value at a dotted config key such as `features.apps`. Origins name a structured feature set
/// to a plain bool (`multi_agent_v2 = true`) as `features.multi_agent_v2.enabled`.
fn value_at<'a>(values: &'a serde_json::Value, key: &str) -> Option<&'a serde_json::Value> {
    let lookup = |key: &str| {
        key.split('.')
            .try_fold(values, |value, part| value.get(part))
    };
    lookup(key).or_else(|| {
        key.strip_suffix(".enabled")
            .and_then(lookup)
            .filter(|value| value.is_boolean())
    })
}

/// Mirrors the effective values this folder resolves to after a save.
fn adopt_effective_switch_state(
    config: &mut Config,
    switch: SettingsSwitch,
    effective_config: &ConfigReadResponse,
) {
    let features = features_from_effective_config(effective_config);
    let adopt_feature = |config: &mut Config, feature: Feature| {
        let enabled = features.enabled(feature);
        if let Err(err) = config.features.set_enabled(feature, enabled) {
            tracing::warn!(
                error = %err,
                feature = feature.key(),
                "failed to sync effective feature state after a settings save"
            );
        }
    };
    for feature in switch.features() {
        adopt_feature(config, *feature);
    }
    match switch {
        SettingsSwitch::Subagents => {
            adopt_feature(config, Feature::MultiAgentV2);
            config.agents_enabled = effective_config
                .config
                .additional
                .get("agents")
                .and_then(|agents| agents.get("enabled"))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(/*default*/ true);
        }
        SettingsSwitch::WebSearch => {
            let mode = effective_config
                .config
                .web_search
                .or_else(|| crate::settings_panel::enabled_web_search_mode(config))
                .unwrap_or(WebSearchMode::Disabled);
            if let Err(err) = config.web_search_mode.set(mode) {
                tracing::warn!(
                    error = %err,
                    "failed to sync effective web search mode after a settings save"
                );
            }
        }
        SettingsSwitch::PluginsAndApps
        | SettingsSwitch::Skills
        | SettingsSwitch::Hooks
        | SettingsSwitch::AgentsMd
        | SettingsSwitch::Memories
        | SettingsSwitch::ImageGeneration
        | SettingsSwitch::Goals => {}
    }
}

#[cfg(test)]
#[path = "settings_panel_tests.rs"]
mod tests;
