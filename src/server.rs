use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::{tool, tool_handler, tool_router, schemars, ServerHandler};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::discovery::ControllerRegistry;
use crate::models::{validate_segments, SegmentInput, StateInput};
use crate::wled::WledClient;

#[derive(Clone)]
pub struct WledServer {
    tool_router: ToolRouter<Self>,
    registry: Arc<ControllerRegistry>,
    client: Arc<WledClient>,
}

// --- Tool parameter types ---

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListControllersRequest {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListPresetsRequest {
    #[schemars(description = "Controller ID (MAC address) from list_controllers")]
    pub controller_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetPresetRequest {
    #[schemars(description = "Controller ID (MAC address) from list_controllers")]
    pub controller_id: String,

    #[schemars(description = "Preset ID number from list_presets")]
    pub preset_id: u16,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetStateRequest {
    #[schemars(description = "Controller ID (MAC address) from list_controllers")]
    pub controller_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetStateRequest {
    #[schemars(description = "Controller ID (MAC address) from list_controllers")]
    pub controller_id: String,

    #[schemars(description = "Turn the controller on (true) or off (false)")]
    pub on: Option<bool>,

    #[schemars(description = "Master brightness 0-255")]
    pub bri: Option<u8>,

    #[schemars(description = "Transition time in 100ms units (e.g. 10 = 1 second crossfade)")]
    pub transition: Option<u16>,

    #[schemars(description = "Segment configurations. Each defines LED range, colors, and effects.")]
    pub segments: Option<Vec<SegmentInput>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SavePresetRequest {
    #[schemars(description = "Controller ID (MAC address) from list_controllers")]
    pub controller_id: String,

    #[schemars(description = "Human-readable name for the preset")]
    pub name: String,

    #[schemars(description = "Turn the controller on (true) or off (false)")]
    pub on: Option<bool>,

    #[schemars(description = "Master brightness 0-255")]
    pub bri: Option<u8>,

    #[schemars(description = "Segment configurations to save")]
    pub segments: Option<Vec<SegmentInput>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetTimersRequest {
    #[schemars(description = "Controller ID (MAC address) from list_controllers")]
    pub controller_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetTimerRequest {
    #[schemars(description = "Controller ID (MAC address) from list_controllers")]
    pub controller_id: String,

    #[schemars(description = "Timer index (0-9). Use get_timers to see current timers and their indices. Indices 8 and 9 are typically sunrise and sunset.")]
    pub index: u8,

    #[schemars(description = "Enable or disable this timer")]
    pub enabled: Option<bool>,

    #[schemars(description = "Hour 0-23 for a specific time, or 255 for sunrise/sunset")]
    pub hour: Option<u8>,

    #[schemars(description = "Minute 0-59 for specific time, or offset -120 to 120 for sunrise/sunset timers")]
    pub minute: Option<i16>,

    #[schemars(description = "Preset ID to activate when this timer fires (0-250)")]
    pub preset_id: Option<u16>,

    #[schemars(description = "Weekday bitmask: bit0=Mon, bit1=Tue, ..., bit6=Sun. 127 = every day.")]
    pub weekdays: Option<u8>,
}

// --- Tool implementations ---

#[tool_router]
impl WledServer {
    pub fn new() -> Self {
        let client = Arc::new(WledClient::new());
        let registry = Arc::new(ControllerRegistry::new(client.clone()));
        Self {
            tool_router: Self::tool_router(),
            registry,
            client,
        }
    }

    #[tool(description = "Discover WLED LED controllers on the local network. Returns a list of controllers with their IDs, names, LED counts, and current state. Call this first to get controller IDs for other tools.")]
    async fn list_controllers(
        &self,
        Parameters(_req): Parameters<ListControllersRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let controllers = self.registry.discover().await.map_err(|e| {
            ErrorData::internal_error(format!("Discovery failed: {e}"), None)
        })?;

        if controllers.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No WLED controllers found on the network. Make sure your WLED devices are powered on and connected to the same network.",
            )]));
        }

        let mut output = format!("Found {} WLED controller(s):\n", controllers.len());

        for (i, c) in controllers.iter().enumerate() {
            let state_summary = match self.client.get_full_json(c).await {
                Ok(full) => {
                    let s = &full.state;
                    let seg_count = s.seg.len();
                    let colors: Vec<String> = s
                        .seg
                        .iter()
                        .filter(|seg| seg.on)
                        .flat_map(|seg| seg.col.first())
                        .map(|c| format!("[{}]", c.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(",")))
                        .collect();
                    format!(
                        "{}, brightness {}, {} segment(s), colors: {}",
                        if s.on { "ON" } else { "OFF" },
                        s.bri,
                        seg_count,
                        if colors.is_empty() { "none".to_string() } else { colors.join(", ") },
                    )
                }
                Err(_) => "unable to fetch state".to_string(),
            };

            output.push_str(&format!(
                "\n{}. {}\n   ID: {}\n   IP: {}\n   LEDs: {} ({})\n   Firmware: {}\n   Max segments: {}\n   State: {}\n",
                i + 1,
                c.name,
                c.id,
                c.ip,
                c.led_count,
                if c.rgbw { "RGBW" } else { "RGB" },
                c.firmware,
                c.max_segments,
                state_summary,
            ));
        }

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    #[tool(description = "List saved presets on a WLED controller with summary info for each. Shows on/off, brightness, transition, mainseg, and a segment overview (id, start, stop, grp, spc, of, on, name). Use get_preset for the full state of a specific preset.")]
    async fn list_presets(
        &self,
        Parameters(req): Parameters<ListPresetsRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let controller = self.registry.get(&req.controller_id).await.ok_or_else(|| {
            ErrorData::invalid_params(
                format!("Controller '{}' not found. Run list_controllers first.", req.controller_id),
                None,
            )
        })?;

        let presets = self.client.get_presets(&controller).await.map_err(|e| {
            ErrorData::internal_error(format!("Failed to fetch presets: {e}"), None)
        })?;

        let obj = presets.as_object().ok_or_else(|| {
            ErrorData::internal_error("Unexpected presets format".to_string(), None)
        })?;

        let mut entries: Vec<(u16, &serde_json::Map<String, serde_json::Value>)> = Vec::new();
        for (key, value) in obj {
            let id: u16 = match key.parse() {
                Ok(id) => id,
                Err(_) => continue,
            };
            let preset_obj = match value.as_object() {
                Some(o) => o,
                None => continue,
            };
            if preset_obj.is_empty() {
                continue;
            }
            // Skip entries that only have segment stubs with no real data
            if !preset_obj.contains_key("on") && !preset_obj.contains_key("n") {
                continue;
            }
            entries.push((id, preset_obj));
        }

        entries.sort_by_key(|(id, _)| *id);

        if entries.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                format!("No presets saved on '{}'.", controller.name),
            )]));
        }

        let mut output = format!("Presets on '{}' ({} total):\n", controller.name, entries.len());

        for (id, preset) in &entries {
            let name = preset
                .get("n")
                .and_then(|v| v.as_str())
                .unwrap_or("(unnamed)");
            let on = preset.get("on").and_then(|v| v.as_bool());
            let bri = preset.get("bri").and_then(|v| v.as_u64());
            let transition = preset.get("transition").and_then(|v| v.as_u64());
            let mainseg = preset.get("mainseg").and_then(|v| v.as_u64());

            output.push_str(&format!("\n  Preset {id}: \"{name}\""));
            if let Some(on) = on {
                output.push_str(&format!(", {}", if on { "ON" } else { "OFF" }));
            }
            if let Some(bri) = bri {
                output.push_str(&format!(", bri:{bri}"));
            }
            if let Some(transition) = transition {
                output.push_str(&format!(", transition:{transition}"));
            }
            if let Some(mainseg) = mainseg {
                output.push_str(&format!(", mainseg:{mainseg}"));
            }
            output.push('\n');

            if let Some(segs) = preset.get("seg").and_then(|v| v.as_array()) {
                for seg in segs {
                    let seg_obj = match seg.as_object() {
                        Some(o) => o,
                        None => continue,
                    };
                    // Skip stub segments (only have "stop": 0)
                    let stop = seg_obj.get("stop").and_then(|v| v.as_u64()).unwrap_or(0);
                    if stop == 0 && !seg_obj.contains_key("col") {
                        continue;
                    }

                    let seg_id = seg_obj.get("id").and_then(|v| v.as_u64()).map(|v| v.to_string()).unwrap_or_default();
                    let start = seg_obj.get("start").and_then(|v| v.as_u64()).unwrap_or(0);
                    let grp = seg_obj.get("grp").and_then(|v| v.as_u64());
                    let spc = seg_obj.get("spc").and_then(|v| v.as_u64());
                    let of = seg_obj.get("of").and_then(|v| v.as_i64());
                    let seg_on = seg_obj.get("on").and_then(|v| v.as_bool()).unwrap_or(true);
                    let seg_name = seg_obj.get("n").and_then(|v| v.as_str()).unwrap_or("");

                    let mut seg_line = format!(
                        "    seg {seg_id}: {start}-{stop}, {}",
                        if seg_on { "on" } else { "off" },
                    );
                    if let Some(grp) = grp {
                        if grp > 1 { seg_line.push_str(&format!(", grp:{grp}")); }
                    }
                    if let Some(spc) = spc {
                        if spc > 0 { seg_line.push_str(&format!(", spc:{spc}")); }
                    }
                    if let Some(of) = of {
                        if of != 0 { seg_line.push_str(&format!(", of:{of}")); }
                    }
                    if !seg_name.is_empty() {
                        seg_line.push_str(&format!(", n:\"{seg_name}\""));
                    }
                    output.push_str(&seg_line);
                    output.push('\n');
                }
            }
        }

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    #[tool(description = "Get the full JSON state of a specific preset. Use this to see exactly how a preset is configured — colors, segments, effects — as a reference for creating similar states.")]
    async fn get_preset(
        &self,
        Parameters(req): Parameters<GetPresetRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let controller = self.registry.get(&req.controller_id).await.ok_or_else(|| {
            ErrorData::invalid_params(
                format!("Controller '{}' not found. Run list_controllers first.", req.controller_id),
                None,
            )
        })?;

        let presets = self.client.get_presets(&controller).await.map_err(|e| {
            ErrorData::internal_error(format!("Failed to fetch presets: {e}"), None)
        })?;

        let key = req.preset_id.to_string();
        let preset = presets
            .get(&key)
            .ok_or_else(|| {
                ErrorData::invalid_params(
                    format!("Preset {} not found on '{}'.", req.preset_id, controller.name),
                    None,
                )
            })?;

        let pretty = serde_json::to_string_pretty(preset).map_err(|e| {
            ErrorData::internal_error(format!("Failed to format preset: {e}"), None)
        })?;

        let name = preset
            .get("n")
            .and_then(|v| v.as_str())
            .unwrap_or("(unnamed)");

        let output = format!(
            "Preset {} \"{}\" on '{}':\n\n{}",
            req.preset_id, name, controller.name, pretty
        );
        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    #[tool(description = "Get the full current JSON state of a WLED controller. Returns the complete state object (including all segment details with colors, effects, grouping, spacing) and device info (LED count, RGBW capability, max segments). This is the exact JSON shape that set_state accepts.")]
    async fn get_state(
        &self,
        Parameters(req): Parameters<GetStateRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let controller = self.registry.get(&req.controller_id).await.ok_or_else(|| {
            ErrorData::invalid_params(
                format!("Controller '{}' not found. Run list_controllers first.", req.controller_id),
                None,
            )
        })?;

        let full_json = self.client.get_full_json_raw(&controller.ip.to_string(), controller.port).await.map_err(|e| {
            ErrorData::internal_error(format!("Failed to fetch state: {e}"), None)
        })?;

        // Return the raw JSON so the LLM sees the exact shape
        let raw = self.client.get_raw_json(&controller).await.map_err(|e| {
            ErrorData::internal_error(format!("Failed to fetch state: {e}"), None)
        })?;

        let pretty = serde_json::to_string_pretty(&raw).map_err(|e| {
            ErrorData::internal_error(format!("Failed to format state: {e}"), None)
        })?;

        let info = &full_json.info;
        let header = format!(
            "Controller: {} (ID: {})\nLEDs: {} ({}), max segments: {}, firmware: {}\n\nFull state:\n",
            info.name, info.mac, info.leds.count,
            if info.leds.rgbw { "RGBW" } else { "RGB" },
            info.leds.maxseg, info.ver,
        );

        Ok(CallToolResult::success(vec![Content::text(format!("{header}{pretty}"))]))
    }

    #[tool(description = "Set the state of a WLED controller. You can change brightness, on/off, and configure segments with colors. Supports partial updates — only the fields you provide will change. For multi-color patterns, create multiple segments covering different LED ranges. Returns the full resulting JSON state.")]
    async fn set_state(
        &self,
        Parameters(req): Parameters<SetStateRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let controller = self.registry.get(&req.controller_id).await.ok_or_else(|| {
            ErrorData::invalid_params(
                format!("Controller '{}' not found. Run list_controllers first.", req.controller_id),
                None,
            )
        })?;

        if let Some(ref segments) = req.segments {
            validate_segments(segments, Some(controller.led_count)).map_err(|e| {
                ErrorData::invalid_params(e, None)
            })?;
        }

        let state = StateInput {
            on: req.on,
            bri: req.bri,
            transition: req.transition,
            seg: req.segments,
            psave: None,
            n: None,
            v: Some(true),
        };

        self.client.post_state(&controller, &state).await.map_err(|e| {
            ErrorData::internal_error(format!("Failed to set state: {e}"), None)
        })?;

        // Fetch the full resulting state so the LLM sees what happened
        let raw = self.client.get_raw_json(&controller).await.map_err(|e| {
            ErrorData::internal_error(format!("State applied but failed to read back: {e}"), None)
        })?;

        let pretty = serde_json::to_string_pretty(&raw).map_err(|e| {
            ErrorData::internal_error(format!("Failed to format state: {e}"), None)
        })?;

        Ok(CallToolResult::success(vec![Content::text(
            format!("State updated successfully.\n\n{pretty}")
        )]))
    }

    #[tool(description = "Get the time-controlled preset schedule (timers) for a WLED controller. Shows sunrise/sunset triggers and timed preset activations. Each timer has an index, enabled state, time or sunrise/sunset, preset ID, and weekday schedule.")]
    async fn get_timers(
        &self,
        Parameters(req): Parameters<GetTimersRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let controller = self.registry.get(&req.controller_id).await.ok_or_else(|| {
            ErrorData::invalid_params(
                format!("Controller '{}' not found. Run list_controllers first.", req.controller_id),
                None,
            )
        })?;

        let config = self.client.get_config(&controller).await.map_err(|e| {
            ErrorData::internal_error(format!("Failed to fetch config: {e}"), None)
        })?;

        let timers = config
            .get("timers")
            .and_then(|t| t.get("ins"))
            .and_then(|ins| ins.as_array());

        let timers = match timers {
            Some(t) => t,
            None => {
                return Ok(CallToolResult::success(vec![Content::text(
                    format!("No timers configured on '{}'.", controller.name),
                )]));
            }
        };

        let mut output = format!("Timers on '{}' ({} slots):\n\n", controller.name, timers.len());

        for (i, timer) in timers.iter().enumerate() {
            let en = timer.get("en").and_then(|v| v.as_u64()).unwrap_or(0);
            let hour = timer.get("hour").and_then(|v| v.as_u64()).unwrap_or(0) as u8;
            let min = timer.get("min").and_then(|v| v.as_i64()).unwrap_or(0);
            let macro_id = timer.get("macro").and_then(|v| v.as_u64()).unwrap_or(0);
            let dow = timer.get("dow").and_then(|v| v.as_u64()).unwrap_or(0) as u8;

            // Skip completely unconfigured timers (disabled, no preset, no time)
            if en == 0 && macro_id == 0 && hour == 0 && min == 0 {
                continue;
            }

            let enabled_str = if en == 1 { "ENABLED" } else { "disabled" };

            let time_str = if hour == 255 {
                // Determine sunrise vs sunset by index convention
                // WLED UI: index 8 = sunrise, index 9 = sunset
                // But in the ins[] array, any entry with hour=255 is sun-based.
                // We use a heuristic: first hour=255 entry = sunrise, second = sunset
                let label = format!("Sunrise/Sunset (hour=255)");
                if min == 0 {
                    label
                } else if min > 0 {
                    format!("{label} +{min}min")
                } else {
                    format!("{label} {min}min")
                }
            } else {
                format!("{:02}:{:02}", hour, min)
            };

            let days = format_weekdays(dow);

            output.push_str(&format!(
                "  [{i}] {enabled_str} | {time_str} | preset: {macro_id} | days: {days}\n"
            ));
        }

        output.push_str("\nNote: hour=255 means sunrise or sunset. WLED determines which based on the timer index (typically index 8=sunrise, 9=sunset in the UI, but in the API array they appear in order).");

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    #[tool(description = "Update a time-controlled preset timer on a WLED controller. Use this to change which preset activates at sunrise/sunset, enable/disable timers, or set specific times. Only the fields you provide will be changed. Use get_timers first to see current timer indices.")]
    async fn set_timer(
        &self,
        Parameters(req): Parameters<SetTimerRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let controller = self.registry.get(&req.controller_id).await.ok_or_else(|| {
            ErrorData::invalid_params(
                format!("Controller '{}' not found. Run list_controllers first.", req.controller_id),
                None,
            )
        })?;

        if req.index > 9 {
            return Err(ErrorData::invalid_params(
                "Timer index must be 0-9.".to_string(),
                None,
            ));
        }

        // Read current config
        let config = self.client.get_config(&controller).await.map_err(|e| {
            ErrorData::internal_error(format!("Failed to fetch config: {e}"), None)
        })?;

        let mut timers_ins = config
            .get("timers")
            .and_then(|t| t.get("ins"))
            .and_then(|ins| ins.as_array())
            .cloned()
            .unwrap_or_default();

        // Extend array if needed
        while timers_ins.len() <= req.index as usize {
            timers_ins.push(serde_json::json!({
                "en": 0, "hour": 0, "min": 0, "macro": 0, "dow": 127
            }));
        }

        let entry = &mut timers_ins[req.index as usize];
        let obj = entry.as_object_mut().ok_or_else(|| {
            ErrorData::internal_error("Invalid timer entry format".to_string(), None)
        })?;

        // Apply partial updates
        if let Some(enabled) = req.enabled {
            obj.insert("en".to_string(), serde_json::json!(if enabled { 1 } else { 0 }));
        }
        if let Some(hour) = req.hour {
            obj.insert("hour".to_string(), serde_json::json!(hour));
        }
        if let Some(minute) = req.minute {
            obj.insert("min".to_string(), serde_json::json!(minute));
        }
        if let Some(preset_id) = req.preset_id {
            obj.insert("macro".to_string(), serde_json::json!(preset_id));
        }
        if let Some(weekdays) = req.weekdays {
            obj.insert("dow".to_string(), serde_json::json!(weekdays));
        }

        // POST the full timers array back
        let update = serde_json::json!({
            "timers": {
                "ins": timers_ins
            }
        });

        self.client.post_config(&controller, &update).await.map_err(|e| {
            ErrorData::internal_error(format!("Failed to update timer: {e}"), None)
        })?;

        // Read back and show the updated timer
        let new_config = self.client.get_config(&controller).await.map_err(|e| {
            ErrorData::internal_error(format!("Timer updated but failed to read back: {e}"), None)
        })?;

        let new_timer = new_config
            .get("timers")
            .and_then(|t| t.get("ins"))
            .and_then(|ins| ins.get(req.index as usize));

        let timer_display = match new_timer {
            Some(t) => serde_json::to_string_pretty(t).unwrap_or_default(),
            None => "(unable to read back)".to_string(),
        };

        let output = format!(
            "Timer {} updated on '{}'.\n\nCurrent value:\n{}",
            req.index, controller.name, timer_display
        );
        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    #[tool(description = "Save the current or provided state as a named preset on a WLED controller. If segments are provided, they will be applied and saved. If no segments are provided, the current state is saved.")]
    async fn save_preset(
        &self,
        Parameters(req): Parameters<SavePresetRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let controller = self.registry.get(&req.controller_id).await.ok_or_else(|| {
            ErrorData::invalid_params(
                format!("Controller '{}' not found. Run list_controllers first.", req.controller_id),
                None,
            )
        })?;

        if let Some(ref segments) = req.segments {
            validate_segments(segments, Some(controller.led_count)).map_err(|e| {
                ErrorData::invalid_params(e, None)
            })?;
        }

        // Find next available preset ID
        let presets = self.client.get_presets(&controller).await.map_err(|e| {
            ErrorData::internal_error(format!("Failed to fetch presets: {e}"), None)
        })?;

        let used_ids: std::collections::HashSet<u16> = presets
            .as_object()
            .map(|obj| {
                obj.keys()
                    .filter_map(|k| k.parse::<u16>().ok())
                    .filter(|id| {
                        obj.get(&id.to_string())
                            .and_then(|v| v.as_object())
                            .map(|o| !o.is_empty() && o.contains_key("n"))
                            .unwrap_or(false)
                    })
                    .collect()
            })
            .unwrap_or_default();

        let preset_id = (1u16..=250)
            .find(|id| !used_ids.contains(id))
            .ok_or_else(|| {
                ErrorData::internal_error("No available preset slots (all 250 in use)".to_string(), None)
            })?;

        let state = StateInput {
            on: req.on,
            bri: req.bri,
            transition: None,
            seg: req.segments,
            psave: Some(preset_id),
            n: Some(req.name.clone()),
            v: Some(true),
        };

        self.client.post_state(&controller, &state).await.map_err(|e| {
            ErrorData::internal_error(format!("Failed to save preset: {e}"), None)
        })?;

        let output = format!(
            "Preset saved successfully!\n  ID: {}\n  Name: \"{}\"\n  Controller: {}",
            preset_id, req.name, controller.name,
        );
        Ok(CallToolResult::success(vec![Content::text(output)]))
    }
}

#[tool_handler]
impl ServerHandler for WledServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(
                "WLED MCP Server — control WLED LED controllers on your network. \
                 Start with list_controllers to discover devices, then use get_state, \
                 set_state, list_presets, get_preset, and save_preset to view and modify \
                 LED colors and patterns. Use get_timers and set_timer to manage \
                 sunrise/sunset schedules and time-controlled preset activation."
                    .to_string(),
            )
    }
}

fn format_weekdays(dow: u8) -> String {
    if dow == 127 || dow == 255 {
        return "every day".to_string();
    }
    if dow == 0 {
        return "none".to_string();
    }
    let days = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    let active: Vec<&str> = days
        .iter()
        .enumerate()
        .filter(|(i, _)| dow & (1 << i) != 0)
        .map(|(_, d)| *d)
        .collect();
    active.join(", ")
}
