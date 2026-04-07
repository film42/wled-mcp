use std::net::IpAddr;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// --- Input types (for MCP tool parameters and WLED POST requests) ---

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SegmentInput {
    #[schemars(description = "Segment ID (0-indexed). Omit to infer from array position.")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<u16>,

    #[schemars(description = "First LED index in this segment (0-indexed)")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start: Option<u16>,

    #[schemars(
        description = "LED index after the last LED (exclusive). Set to 0 to delete a segment."
    )]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<u16>,

    #[schemars(description = "Segment on/off")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on: Option<bool>,

    #[schemars(description = "Segment brightness 0-255")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bri: Option<u8>,

    #[schemars(
        description = "Up to 3 colors as arrays. RGB: [R,G,B], RGBW: [R,G,B,W]. Values 0-255."
    )]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub col: Option<Vec<Vec<u8>>>,

    #[schemars(description = "Effect ID. Use 0 for solid color.")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fx: Option<u16>,

    #[schemars(description = "Color palette ID. Use 0 for default.")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pal: Option<u16>,

    #[schemars(description = "Grouping: how many consecutive LEDs share the same color")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grp: Option<u8>,

    #[schemars(description = "Spacing: how many LEDs are skipped between groups")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spc: Option<u8>,

    #[schemars(description = "Offset: rotate the virtual start of the segment")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub of: Option<i16>,

    #[schemars(description = "Segment name")]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "n")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateInput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on: Option<bool>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub bri: Option<u8>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub transition: Option<u16>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub seg: Option<Vec<SegmentInput>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub psave: Option<u16>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub n: Option<String>,

    /// Request full state in response
    #[serde(skip_serializing_if = "Option::is_none")]
    pub v: Option<bool>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub o: Option<bool>,
}

// --- Response types (parsed from WLED GET responses) ---

#[derive(Debug, Clone, Deserialize)]
pub struct WledFullJson {
    pub state: StateResponse,
    pub info: InfoResponse,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct InfoResponse {
    pub ver: String,
    pub mac: String,
    pub name: String,
    pub leds: LedsInfo,
    #[serde(default)]
    pub ip: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LedsInfo {
    pub count: u32,
    #[serde(default)]
    pub rgbw: bool,
    #[serde(default)]
    pub maxseg: u8,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct StateResponse {
    pub on: bool,
    pub bri: u8,
    #[serde(default)]
    pub seg: Vec<SegmentResponse>,
    #[serde(default)]
    pub ps: Option<i16>,
    #[serde(default)]
    pub mainseg: Option<u8>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct SegmentResponse {
    pub id: u16,
    pub start: u16,
    pub stop: u16,
    #[serde(default = "default_true")]
    pub on: bool,
    #[serde(default = "default_max_brightness")]
    pub bri: u8,
    #[serde(default)]
    pub col: Vec<Vec<u8>>,
    #[serde(default)]
    pub fx: u16,
    #[serde(default)]
    pub pal: u16,
    #[serde(default)]
    #[serde(rename = "n")]
    pub name: Option<String>,
    #[serde(default)]
    pub grp: Option<u8>,
    #[serde(default)]
    pub spc: Option<u8>,
    #[serde(default)]
    pub of: Option<i16>,
}

fn default_true() -> bool {
    true
}

fn default_max_brightness() -> u8 {
    255
}

// --- Internal types ---

#[derive(Debug, Clone)]
pub struct ControllerInfo {
    pub id: String,
    pub name: String,
    pub ip: IpAddr,
    pub port: u16,
    pub led_count: u32,
    pub rgbw: bool,
    pub firmware: String,
    pub max_segments: u8,
}

// --- Validation ---

pub fn validate_segments(segments: &[SegmentInput], led_count: Option<u32>) -> Result<(), String> {
    for (i, seg) in segments.iter().enumerate() {
        if let Some(ref col) = seg.col {
            if col.len() > 3 {
                return Err(format!(
                    "Segment {i}: at most 3 colors allowed, got {}",
                    col.len()
                ));
            }
            for (j, color) in col.iter().enumerate() {
                if color.len() != 3 && color.len() != 4 {
                    return Err(format!(
                        "Segment {i}, color {j}: must be [R,G,B] or [R,G,B,W], got {} values",
                        color.len()
                    ));
                }
            }
        }

        if let (Some(start), Some(stop)) = (seg.start, seg.stop)
            && stop > 0
            && stop <= start
        {
            return Err(format!(
                "Segment {i}: stop ({stop}) must be greater than start ({start})"
            ));
        }

        if let Some(stop) = seg.stop
            && let Some(count) = led_count
            && stop > count as u16
        {
            return Err(format!(
                "Segment {i}: stop ({stop}) exceeds LED count ({count})"
            ));
        }
    }
    Ok(())
}
