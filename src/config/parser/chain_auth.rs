use super::{
    chain::{children, invalid},
    values::at,
};
use crate::{
    auth::model::{Basic, Secret, Token, User},
    config::{
        error::Result,
        source::{Located, Origin},
    },
    http_policy::{headers::protected, model::Action},
};
use kdl::KdlNode;
use std::collections::BTreeSet;

pub(super) fn parse(node: &KdlNode, origin: &Origin) -> Result<Action> {
    let item = if node.name().value() == "basic-auth" {
        "basic-auth"
    } else {
        "static-token"
    };
    let mut realm = None;
    let mut header = None;
    let mut file = None;
    if node.ty().is_some() {
        return Err(invalid("CONFIG_UNKNOWN", item, origin));
    }
    for e in node.entries() {
        if e.ty().is_some() {
            return Err(invalid("CONFIG_UNKNOWN", item, origin));
        }
        let v = e
            .value()
            .as_string()
            .ok_or_else(|| invalid("CONFIG_TYPE", item, origin))?;
        match (item, e.name().map(kdl::KdlIdentifier::value)) {
            ("basic-auth", Some("realm")) if realm.is_none() => realm = Some(v),
            ("static-token", Some("header")) if header.is_none() => header = Some(v),
            ("static-token", Some("file")) if file.is_none() => file = Some(v),
            _ => return Err(invalid("CONFIG_UNKNOWN", item, origin)),
        }
    }
    if item == "static-token" {
        if node.children().is_some() {
            return Err(invalid("CONFIG_UNKNOWN", item, origin));
        }
        let header = header
            .and_then(|v| v.parse::<http::HeaderName>().ok())
            .ok_or_else(|| invalid("AUTH_PROTECTED_HEADER", item, origin))?;
        if protected(&header, &[]) {
            return Err(invalid("AUTH_PROTECTED_HEADER", item, origin));
        }
        let file = file
            .filter(|v| !v.is_empty())
            .ok_or_else(|| invalid("AUTH_INVALID_CREDENTIAL", item, origin))?;
        return Ok(Action::StaticToken(Token {
            header,
            file: Located::new(origin.relative_path(file), origin.clone()),
            origin: origin.clone(),
        }));
    }
    let realm = realm
        .filter(|s| !s.is_empty() && !s.chars().any(char::is_control))
        .ok_or_else(|| invalid("AUTH_INVALID_CREDENTIAL", item, origin))?;
    let mut challenge = String::from("Basic realm=\"");
    for c in realm.chars() {
        if matches!(c, '\\' | '"') {
            challenge.push('\\');
        }
        challenge.push(c);
    }
    challenge.push('"');
    if http::HeaderValue::from_str(&challenge).is_err() {
        return Err(invalid("AUTH_INVALID_CREDENTIAL", item, origin));
    }
    if node.children().is_none() {
        return Err(invalid("CONFIG_TYPE", item, origin));
    }
    let mut users = Vec::new();
    let mut names = BTreeSet::new();
    for user in children(node) {
        let loc = at(user, origin);
        if user.name().value() != "user" || user.ty().is_some() || user.children().is_some() {
            return Err(invalid("CONFIG_UNKNOWN", item, &loc));
        }
        let mut name = None;
        let mut hash = None;
        for e in user.entries() {
            if e.ty().is_some() {
                return Err(invalid("CONFIG_UNKNOWN", item, &loc));
            }
            let v = e
                .value()
                .as_string()
                .ok_or_else(|| invalid("CONFIG_TYPE", item, &loc))?;
            match e.name().map(kdl::KdlIdentifier::value) {
                None if name.is_none() => name = Some(v),
                Some("hash") if hash.is_none() => hash = Some(v),
                _ => return Err(invalid("CONFIG_UNKNOWN", item, &loc)),
            }
        }
        let name = name
            .filter(|s| !s.is_empty() && !s.contains(':') && !s.chars().any(char::is_control))
            .ok_or_else(|| invalid("AUTH_INVALID_CREDENTIAL", item, &loc))?;
        if !names.insert(name.to_owned()) {
            return Err(invalid("AUTH_INVALID_CREDENTIAL", item, &loc));
        }
        let hash = hash.ok_or_else(|| invalid("AUTH_INVALID_HASH", item, &loc))?;
        if !crate::auth::hash::valid(hash) {
            return Err(invalid("AUTH_INVALID_HASH", item, &loc));
        }
        users.push(User {
            name: name.to_owned(),
            hash: Secret(hash.to_owned()),
            origin: loc,
        });
    }
    if users.is_empty() {
        return Err(invalid("AUTH_INVALID_CREDENTIAL", item, origin));
    }
    Ok(Action::BasicAuth(Basic {
        realm: realm.to_owned(),
        challenge,
        users,
        origin: origin.clone(),
    }))
}
