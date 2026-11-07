use super::chain::invalid;
use crate::{
    config::{
        error::Result,
        source::{Located, Origin},
    },
    http_policy::model::Action,
    webdav::model::WebDav,
};
use kdl::KdlNode;

pub(super) fn parse(node: &KdlNode, origin: &Origin) -> Result<Action> {
    if node.ty().is_some() || node.children().is_some() {
        return Err(invalid("CONFIG_UNKNOWN", "webdav", origin));
    }
    let mut root = None;
    let mut storage_id = None;
    for entry in node.entries() {
        if entry.ty().is_some() {
            return Err(invalid("CONFIG_UNKNOWN", "webdav", origin));
        }
        let value = entry
            .value()
            .as_string()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid("CONFIG_TYPE", "webdav", origin))?;
        match entry.name().map(kdl::KdlIdentifier::value) {
            Some("root") if root.is_none() => {
                root = Some(Located::new(origin.relative_path(value), origin.clone()))
            }
            Some("storage-id") if storage_id.is_none() => storage_id = Some(value.to_owned()),
            _ => return Err(invalid("CONFIG_UNKNOWN", "webdav", origin)),
        }
    }
    Ok(Action::WebDav(WebDav {
        root: root.ok_or_else(|| invalid("CONFIG_TYPE", "root", origin))?,
        storage_id: storage_id.ok_or_else(|| invalid("CONFIG_TYPE", "storage-id", origin))?,
        origin: origin.clone(),
    }))
}
