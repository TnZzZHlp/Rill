use std::path::PathBuf;

use kdl::KdlNode;
use serde::ser::{Serialize, SerializeMap, Serializer};

use crate::config::{
    ConfigError, Result,
    source::{Located, Origin},
};

const MAX_RECORD_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LogField {
    Timestamp,
    Site,
    Route,
    Method,
    Status,
    DurationMs,
    Bytes,
    Path,
    ClientIp,
}

impl LogField {
    const fn bit(self) -> u16 {
        1 << self as u16
    }

    fn parse(value: &str, origin: &Origin) -> Result<Self> {
        match value {
            "timestamp" => Ok(Self::Timestamp),
            "site" => Ok(Self::Site),
            "route" => Ok(Self::Route),
            "method" => Ok(Self::Method),
            "status" => Ok(Self::Status),
            "duration-ms" => Ok(Self::DurationMs),
            "bytes" => Ok(Self::Bytes),
            "path" => Ok(Self::Path),
            "client-ip" => Ok(Self::ClientIp),
            _ => Err(ConfigError::new(
                "LOG_FIELD_FORBIDDEN",
                "field",
                "log field is not permitted",
                origin,
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LogFields(u16);

impl Default for LogFields {
    fn default() -> Self {
        Self((1 << 7) - 1)
    }
}
impl LogFields {
    pub const fn contains(self, field: LogField) -> bool {
        self.0 & field.bit() != 0
    }
}

#[derive(Clone, Debug)]
pub struct LogConfig {
    pub file: Located<PathBuf>,
    pub rotate_size: u64,
    pub retain_files: u8,
    pub retain_days: u8,
    pub fields: LogFields,
    pub origin: Origin,
}

fn invalid(code: &'static str, item: &'static str, origin: &Origin) -> ConfigError {
    ConfigError::new(code, item, "invalid file access log configuration", origin)
}

pub fn parse_site_label(node: &KdlNode, origin: &Origin) -> Result<Located<String>> {
    if node.ty().is_some()
        || node.children().is_some()
        || node.entries().len() != 1
        || node.entries()[0].name().is_some()
        || node.entries()[0].ty().is_some()
    {
        return Err(invalid("CONFIG_TYPE", "site-label", origin));
    }
    let label = node.entries()[0]
        .value()
        .as_string()
        .filter(|value| !value.is_empty() && !value.chars().any(char::is_control))
        .ok_or_else(|| invalid("CONFIG_TYPE", "site-label", origin))?;
    Ok(Located::new(label.to_owned(), origin.clone()))
}

pub fn parse_access_log(node: &KdlNode, origin: &Origin) -> Result<LogConfig> {
    if node.ty().is_some() {
        return Err(invalid("CONFIG_UNKNOWN", "access-log", origin));
    }
    let mut file = None;
    let mut rotate_size = None;
    let mut retain_files = None;
    let mut retain_days = None;
    for entry in node.entries() {
        if entry.ty().is_some() {
            return Err(invalid("CONFIG_UNKNOWN", "access-log", origin));
        }
        match entry.name().map(kdl::KdlIdentifier::value) {
            Some("file") if file.is_none() => {
                let value = entry
                    .value()
                    .as_string()
                    .filter(|value| !value.is_empty() && !value.chars().any(char::is_control))
                    .ok_or_else(|| invalid("CONFIG_TYPE", "file", origin))?;
                file = Some(Located::new(origin.relative_path(value), origin.clone()));
            }
            Some("rotate-size") if rotate_size.is_none() => {
                rotate_size = Some(number(entry, 16 * 1024, 10 * 1024 * 1024, origin)?)
            }
            Some("retain-files") if retain_files.is_none() => {
                retain_files = Some(number(entry, 1, 5, origin)?)
            }
            Some("retain-days") if retain_days.is_none() => {
                retain_days = Some(number(entry, 1, 7, origin)?)
            }
            _ => return Err(invalid("CONFIG_UNKNOWN", "access-log", origin)),
        }
    }
    let mut fields = LogFields(0);
    let mut has_field = false;
    for child in node.children().into_iter().flat_map(|doc| doc.nodes()) {
        let child_origin = Origin {
            offset: child.span().offset(),
            length: child.span().len(),
            ..origin.clone()
        };
        if child.name().value() != "field" {
            return Err(invalid("CONFIG_UNKNOWN", "access-log", &child_origin));
        }
        let value = parse_field(child, &child_origin)?;
        fields.0 |= value.bit();
        has_field = true;
    }
    Ok(LogConfig {
        file: file.ok_or_else(|| invalid("CONFIG_TYPE", "file", origin))?,
        rotate_size: rotate_size.ok_or_else(|| invalid("CONFIG_TYPE", "rotate-size", origin))?,
        retain_files: u8::try_from(
            retain_files.ok_or_else(|| invalid("CONFIG_TYPE", "retain-files", origin))?,
        )
        .map_err(|_| invalid("LOG_LIMIT", "retain-files", origin))?,
        retain_days: u8::try_from(
            retain_days.ok_or_else(|| invalid("CONFIG_TYPE", "retain-days", origin))?,
        )
        .map_err(|_| invalid("LOG_LIMIT", "retain-days", origin))?,
        fields: if has_field {
            fields
        } else {
            LogFields::default()
        },
        origin: origin.clone(),
    })
}

fn number(entry: &kdl::KdlEntry, min: u64, max: u64, origin: &Origin) -> Result<u64> {
    entry
        .value()
        .as_integer()
        .and_then(|v| u64::try_from(v).ok())
        .filter(|v| (min..=max).contains(v))
        .ok_or_else(|| invalid("LOG_LIMIT", "access-log", origin))
}

fn parse_field(node: &KdlNode, origin: &Origin) -> Result<LogField> {
    if node.ty().is_some()
        || node.children().is_some()
        || node.entries().len() != 1
        || node.entries()[0].name().is_some()
        || node.entries()[0].ty().is_some()
    {
        return Err(invalid("CONFIG_TYPE", "field", origin));
    }
    let value = node.entries()[0]
        .value()
        .as_string()
        .ok_or_else(|| invalid("CONFIG_TYPE", "field", origin))?;
    LogField::parse(value, origin)
}

pub fn validate_candidate_count(count: usize, origin: &Origin) -> Result<()> {
    if count > 64 {
        Err(invalid("LOG_CAPACITY", "access-log", origin))
    } else {
        Ok(())
    }
}

pub fn site_identity<'a>(label: Option<&'a str>, normalized_domain: &'a str) -> &'a str {
    label.unwrap_or(normalized_domain)
}

pub fn legacy_route_identity(route: &crate::config::model::Route) -> &str {
    &route.pattern.value
}

pub fn rule_identity(id: Option<&str>, index: usize) -> String {
    id.map_or_else(|| format!("rule-{}", index + 1), str::to_owned)
}

pub struct FileLogRecord<'a> {
    pub fields: LogFields,
    pub timestamp: u64,
    pub site: Option<&'a str>,
    pub route: Option<&'a str>,
    pub method: &'a str,
    pub status: Option<u16>,
    pub duration_ms: u64,
    pub bytes: Option<u64>,
    pub uri: Option<&'a http::Uri>,
    pub client_ip: Option<std::net::IpAddr>,
}

impl Serialize for FileLogRecord<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        macro_rules! field {
            ($kind:ident, $name:literal, $value:expr) => {
                if self.fields.contains(LogField::$kind) {
                    map.serialize_entry($name, &$value)?;
                }
            };
        }
        field!(Timestamp, "timestamp", self.timestamp);
        field!(Site, "site", self.site);
        field!(Route, "route", self.route);
        field!(Method, "method", self.method);
        field!(Status, "status", self.status);
        field!(DurationMs, "duration-ms", self.duration_ms);
        field!(Bytes, "bytes", self.bytes);
        field!(Path, "path", self.uri.map(http::Uri::path));
        field!(ClientIp, "client-ip", self.client_ip);
        map.end()
    }
}

struct BoundedRecord(Vec<u8>);

impl std::io::Write for BoundedRecord {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_RECORD_BYTES - 1 - self.0.len() {
            return Err(std::io::Error::other("log record exceeds limit"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub fn encode_record(
    record: &FileLogRecord<'_>,
) -> std::result::Result<Vec<u8>, serde_json::Error> {
    let mut output = BoundedRecord(Vec::with_capacity(MAX_RECORD_BYTES));
    serde_json::to_writer(&mut output, record)?;
    output.0.push(b'\n');
    Ok(output.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn parse(text: &str) -> Result<LogConfig> {
        let doc = kdl::KdlDocument::parse_v2(text).unwrap();
        parse_access_log(&doc.nodes()[0], &Origin::file(Path::new("/tmp/config.kdl")))
    }

    #[test]
    fn file_log_limits_and_whitelist() {
        let config =
            parse("access-log file=\"log\" rotate-size=16384 retain-files=1 retain-days=7")
                .unwrap();
        assert_eq!(config.file.value, Path::new("/tmp/log"));
        assert!(config.fields.contains(LogField::Route));
        assert!(!config.fields.contains(LogField::Path));
        assert!(!config.fields.contains(LogField::ClientIp));
        assert_eq!(
            parse("access-log file=\"log\" rotate-size=16383 retain-files=1 retain-days=1")
                .unwrap_err()
                .code,
            "LOG_LIMIT"
        );
        assert_eq!(parse("access-log file=\"log\" rotate-size=16384 retain-files=1 retain-days=1 { field \"authorization\"; }").unwrap_err().code, "LOG_FIELD_FORBIDDEN");
        assert_eq!(
            validate_candidate_count(65, &config.origin)
                .unwrap_err()
                .code,
            "LOG_CAPACITY"
        );
    }

    #[test]
    fn record_encoding_is_bounded_and_escapes_input() {
        let config = parse("access-log file=\"log\" rotate-size=16384 retain-files=1 retain-days=1 { field \"site\"; field \"path\"; }").unwrap();
        let uri = "/ok?secret=hidden".parse().unwrap();
        let mut record = FileLogRecord {
            fields: config.fields,
            timestamp: 0,
            site: Some("one\n\"two"),
            route: None,
            method: "GET",
            status: Some(200),
            duration_ms: 1,
            bytes: None,
            uri: Some(&uri),
            client_ip: None,
        };
        let bytes = encode_record(&record).unwrap();
        assert_eq!(bytes.iter().filter(|&&byte| byte == b'\n').count(), 1);
        let decoded: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(decoded["site"], "one\n\"two");
        assert!(decoded.get("route").is_none());
        assert_eq!(decoded["path"], "/ok");
        assert!(!String::from_utf8_lossy(&bytes).contains("hidden"));
        let huge = format!("/{}", "a".repeat(MAX_RECORD_BYTES))
            .parse()
            .unwrap();
        record.uri = Some(&huge);
        assert!(encode_record(&record).is_err());
    }
}
