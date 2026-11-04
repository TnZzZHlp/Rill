use std::fmt::Write as _;
use std::io::{self, Write};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug)]
pub enum Level {
    Error,
    Warn,
    Info,
    Debug,
}

#[derive(Clone, Debug)]
pub struct Logger {
    threshold: u8,
    sink: Option<super::access::AccessSink>,
}

fn quoted(value: &str) -> String {
    let mut result = String::from("\"");
    for character in value.chars().take(1024) {
        match character {
            '"' => result.push_str("\\\""),
            '\\' => result.push_str("\\\\"),
            character if character.is_control() => {
                let _ = write!(result, "\\u{:04x}", u32::from(character));
            }
            character => result.push(character),
        }
    }
    result.push('"');
    result
}

impl Logger {
    pub fn new(level: &str) -> Self {
        Self {
            sink: None,
            threshold: match level {
                "off" => 0,
                "error" => 1,
                "warn" => 2,
                "info" => 3,
                "debug" | "trace" => 4,
                _ => 3,
            },
        }
    }

    pub fn with_sink(mut self, sink: super::access::AccessSink) -> Self {
        self.sink = Some(sink);
        self
    }

    pub fn event(&self, level: Level, event: &'static str, fields: &[(&str, &str)]) {
        let (severity, name) = match level {
            Level::Error => (1, "error"),
            Level::Warn => (2, "warn"),
            Level::Info => (3, "info"),
            Level::Debug => (4, "debug"),
        };
        if severity > self.threshold {
            return;
        }
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        if let Some(sink) = &self.sink {
            sink.submit_runtime(timestamp, name, event, fields);
            return;
        }
        let mut record = format!(
            "{{\"time_ms\":{timestamp},\"level\":{},\"event\":{}",
            quoted(name),
            quoted(event)
        );
        for (key, value) in fields {
            let _ = write!(record, ",{}:{}", quoted(key), quoted(value));
        }
        record.push_str("}\n");
        let _ = io::stderr().lock().write_all(record.as_bytes());
    }
}
