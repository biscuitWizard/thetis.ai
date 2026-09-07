//! Projection ranges hidden from the model while retained in the event log.

use crate::thetis::grip::sys;
use crate::thetis::grip::types::LogLevel;
use serde_json::Value;

pub const HIDDEN_KEY: &str = "__hidden";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Hidden {
    ranges: Vec<(u64, u64)>,
}

impl Hidden {
    /// Parse the gateway-owned JSON record leniently and normalise its ranges.
    pub fn parse(text: &str) -> Hidden {
        let Ok(value) = serde_json::from_str::<Value>(text) else {
            return Hidden::default();
        };
        let Some(raw) = value.get("ranges").and_then(Value::as_array) else {
            return Hidden::default();
        };
        let mut ranges: Vec<(u64, u64)> = raw
            .iter()
            .filter_map(|range| {
                let pair = range.as_array()?;
                if pair.len() != 2 { return None; }
                let a = pair[0].as_u64()?;
                let b = pair[1].as_u64()?;
                Some((a.min(b), a.max(b)))
            })
            .collect();
        ranges.sort_unstable();
        let mut merged: Vec<(u64, u64)> = Vec::new();
        for (from, through) in ranges {
            if let Some(last) = merged.last_mut() {
                if from <= last.1.saturating_add(1) {
                    last.1 = last.1.max(through);
                    continue;
                }
            }
            merged.push((from, through));
        }
        Hidden { ranges: merged }
    }

    pub fn load(session_id: &str) -> Hidden {
        let Some(text) = sys::kv_get(session_id, HIDDEN_KEY) else { return Hidden::default(); };
        if text.trim().is_empty() { return Hidden::default(); }
        if serde_json::from_str::<Value>(&text).is_err() {
            sys::log(LogLevel::Warn, "hidden projection is not valid JSON; ignoring it");
        }
        Hidden::parse(&text)
    }

    pub fn contains(&self, seq: u64) -> bool {
        self.ranges.iter().any(|&(from, through)| seq >= from && seq <= through)
    }

    pub fn intersects(&self, from: u64, through: u64) -> bool {
        let (from, through) = (from.min(through), from.max(through));
        self.ranges.iter().any(|&(a, b)| a <= through && b >= from)
    }

    pub fn is_empty(&self) -> bool { self.ranges.is_empty() }
    pub fn ranges(&self) -> &[(u64, u64)] { &self.ranges }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_schema_and_normalises() {
        let hidden = Hidden::parse(r#"{"ranges":[[9,7],[1,3],[3,6],[20,20],["x",4]]}"#);
        assert_eq!(hidden.ranges(), &[(1, 9), (20, 20)]);
    }

    #[test]
    fn garbage_and_empty_read_as_nothing_hidden() {
        assert!(Hidden::parse("garbage").is_empty());
        assert!(Hidden::parse("").is_empty());
        assert!(Hidden::parse(r#"{"ranges":"no"}"#).is_empty());
    }

    #[test]
    fn contains_and_intersects_are_inclusive() {
        let hidden = Hidden::parse(r#"{"ranges":[[4,8]]}"#);
        assert!(hidden.contains(4));
        assert!(hidden.contains(8));
        assert!(!hidden.contains(9));
        assert!(hidden.intersects(1, 4));
        assert!(hidden.intersects(8, 12));
        assert!(!hidden.intersects(9, 12));
    }
}
