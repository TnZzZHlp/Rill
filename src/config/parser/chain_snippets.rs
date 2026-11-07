use std::collections::BTreeMap;

use kdl::KdlNode;

use super::values::{argument, at, shape, string};
use crate::config::error::{ConfigError, Result};
use crate::config::loader::LoadedNode;
use crate::config::source::Origin;

const MAX_DEPTH: usize = 64;
const MAX_STEPS: usize = 16_384;
const MAX_RULES: usize = 4096;
const MAX_BYTES: usize = 8 * 1024 * 1024;

#[derive(Default)]
pub(super) struct Snippets<'a> {
    definitions: BTreeMap<String, &'a LoadedNode>,
    steps: usize,
    rules: usize,
    bytes: usize,
}

fn error(code: &'static str, message: &'static str, origin: &Origin) -> ConfigError {
    ConfigError::new(code, "chain-snippet", message, origin)
}

fn structure_bytes(node: &KdlNode) -> Option<usize> {
    let mut bytes = std::mem::size_of::<KdlNode>().checked_add(
        node.entries()
            .len()
            .checked_mul(std::mem::size_of::<kdl::KdlEntry>())?,
    )?;
    if let Some(document) = node.children() {
        bytes = bytes.checked_add(std::mem::size_of::<kdl::KdlDocument>())?;
        for child in document.nodes() {
            bytes = bytes.checked_add(structure_bytes(child)?)?;
        }
    }
    Some(bytes)
}

impl<'a> Snippets<'a> {
    pub fn collect(nodes: &'a [LoadedNode]) -> Result<Self> {
        let mut result = Self::default();
        for located in nodes {
            if located.node.name().value() != "chain-snippet" {
                continue;
            }
            shape(&located.node, &located.origin, "chain-snippet", 1..=1, true)?;
            let name = argument(&located.node, 0, &located.origin, "chain-snippet")?;
            if let Some(first) = result.definitions.insert(name.value, located) {
                return Err(error(
                    "DUPLICATE",
                    "chain snippet name already defined",
                    &name.origin,
                )
                .related(&first.origin));
            }
        }
        let mut states = BTreeMap::new();
        for (name, definition) in &result.definitions {
            result.check(name, &definition.origin, &mut states, &mut Vec::new())?;
        }
        Ok(result)
    }

    fn check(
        &self,
        name: &str,
        reference: &Origin,
        states: &mut BTreeMap<String, usize>,
        stack: &mut Vec<Origin>,
    ) -> Result<usize> {
        let mut origin = reference.clone();
        crate::config::snippet::add_chain(&mut origin, stack);
        if stack.len() >= MAX_DEPTH {
            return Err(error(
                "CONFIG_LIMIT",
                "chain snippet nesting limit exceeded",
                &origin,
            ));
        }
        if let Some(height) = states.get(name) {
            if *height == 0 {
                return Err(error(
                    "SNIPPET_REF",
                    "cyclic chain snippet reference",
                    &origin,
                ));
            }
            if stack.len() + height > MAX_DEPTH {
                return Err(error(
                    "CONFIG_LIMIT",
                    "chain snippet nesting limit exceeded",
                    &origin,
                ));
            }
            return Ok(*height);
        }
        let definition = self
            .definitions
            .get(name)
            .ok_or_else(|| error("SNIPPET_REF", "chain snippet does not exist", &origin))?;
        let children: Vec<_> = super::chain::children(&definition.node).collect();
        if children.is_empty() {
            return Err(error(
                "CONFIG_TYPE",
                "chain snippet must not be empty",
                &definition.origin,
            ));
        }
        states.insert(name.to_owned(), 0);
        stack.push(origin);
        let mut height = 1;
        for child in children {
            let loc = at(child, &definition.origin);
            match child.name().value() {
                "rule" => {}
                "use-snippet" => {
                    let target = string(child, &loc, "use-snippet")?;
                    height = height.max(1 + self.check(&target.value, &loc, states, stack)?);
                }
                _ => return Err(super::values::unknown(child, &loc)),
            }
        }
        stack.pop();
        states.insert(name.to_owned(), height);
        Ok(height)
    }

    pub fn validate_definitions(&mut self) -> Result<()> {
        let definitions: Vec<_> = self.definitions.values().copied().collect();
        for definition in definitions {
            let mut nodes = Vec::new();
            self.expand_into(
                super::chain::children(&definition.node),
                &definition.origin,
                &mut vec![definition.origin.clone()],
                &mut nodes,
            )?;
            let parse = |errors| {
                super::chain::parse_rules(
                    nodes.iter().map(|n| (&n.node, &n.origin)),
                    &definition.origin,
                    errors,
                    &mut 0,
                )
            };
            if let Err(original) = parse(false) {
                parse(true).map_err(|_| original)?;
            }
        }
        self.steps = 0;
        self.rules = 0;
        self.bytes = 0;
        Ok(())
    }

    pub fn expand<'n>(
        &mut self,
        nodes: impl IntoIterator<Item = &'n KdlNode>,
        origin: &Origin,
    ) -> Result<Vec<LoadedNode>> {
        let mut output = Vec::new();
        self.expand_into(nodes, origin, &mut Vec::new(), &mut output)?;
        Ok(output)
    }

    fn expand_into<'n>(
        &mut self,
        nodes: impl IntoIterator<Item = &'n KdlNode>,
        parent: &Origin,
        stack: &mut Vec<Origin>,
        output: &mut Vec<LoadedNode>,
    ) -> Result<()> {
        for node in nodes {
            let mut origin = at(node, parent);
            crate::config::snippet::add_chain(&mut origin, stack);
            if !stack.is_empty() || node.name().value() == "use-snippet" {
                self.steps += 1;
            }
            if self.steps > MAX_STEPS {
                return Err(error(
                    "CONFIG_LIMIT",
                    "chain snippet expansion step limit exceeded",
                    &origin,
                ));
            }
            if node.name().value() == "use-snippet" {
                let name = string(node, &origin, "use-snippet")?;
                if stack.len() >= MAX_DEPTH {
                    return Err(error(
                        "CONFIG_LIMIT",
                        "chain snippet nesting limit exceeded",
                        &origin,
                    ));
                }
                let definition =
                    self.definitions.get(&name.value).copied().ok_or_else(|| {
                        error("SNIPPET_REF", "chain snippet does not exist", &origin)
                    })?;
                stack.push(origin);
                self.expand_into(
                    super::chain::children(&definition.node),
                    &definition.origin,
                    stack,
                    output,
                )?;
                stack.pop();
            } else {
                if !stack.is_empty() {
                    let cost = node
                        .to_string()
                        .len()
                        .checked_add(structure_bytes(node).ok_or_else(|| {
                            error(
                                "CONFIG_LIMIT",
                                "chain snippet payload limit exceeded",
                                &origin,
                            )
                        })?)
                        .and_then(|n| {
                            n.checked_add(origin.references.len() * std::mem::size_of::<Origin>())
                        })
                        .ok_or_else(|| {
                            error(
                                "CONFIG_LIMIT",
                                "chain snippet payload limit exceeded",
                                &origin,
                            )
                        })?;
                    self.bytes = self.bytes.checked_add(cost).ok_or_else(|| {
                        error(
                            "CONFIG_LIMIT",
                            "chain snippet payload limit exceeded",
                            &origin,
                        )
                    })?;
                    self.rules += 1;
                    if self.bytes > MAX_BYTES || self.rules > MAX_RULES {
                        return Err(error(
                            "CONFIG_LIMIT",
                            "chain snippet expansion limit exceeded",
                            &origin,
                        ));
                    }
                }
                output.push(LoadedNode {
                    node: node.clone(),
                    origin,
                });
            }
        }
        Ok(())
    }
}
