use super::model::{DavError, Result};
use quick_xml::{Reader, events::Event};
use std::collections::BTreeMap;

pub const DAV: &str = "DAV:";
pub(crate) const XML_NAMESPACE: &str = "http://www.w3.org/XML/1998/namespace";
const XMLNS_NAMESPACE: &str = "http://www.w3.org/2000/xmlns/";
pub const XML_LIMIT: usize = 1024 * 1024;
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct Name {
    pub namespace: String,
    pub local: String,
}
impl Name {
    pub fn dav(local: &str) -> Self {
        Self {
            namespace: DAV.into(),
            local: local.into(),
        }
    }
}
#[derive(Debug)]
pub enum Content {
    Text(String),
    Element(Node),
    Instruction(String),
}
#[derive(Debug)]
pub struct Node {
    pub name: Name,
    pub attributes: Vec<(Name, String)>,
    pub content: Vec<Content>,
    spelling: String,
    attribute_spellings: Vec<String>,
    namespaces: std::sync::Arc<BTreeMap<String, String>>,
    _namespace_memory: super::budget::Memory,
    _memory: Option<super::budget::Memory>,
}
impl Node {
    pub fn elements(&self) -> impl Iterator<Item = &Self> {
        self.content.iter().filter_map(|c| {
            if let Content::Element(n) = c {
                Some(n)
            } else {
                None
            }
        })
    }
    pub fn is(&self, local: &str) -> bool {
        self.name.namespace == DAV && self.name.local == local
    }
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|c| {
                if let Content::Text(t) = c {
                    Some(t.as_str())
                } else {
                    None
                }
            })
            .collect()
    }
    pub fn duplicate(&self) -> Result<Self> {
        parse(self.serialize()?.as_bytes())
    }
    pub fn serialize(&self) -> Result<String> {
        let length = self.serialized_length()?;
        let _credit = super::budget::Memory::acquire(2 * length)?;
        let mut out = String::with_capacity(length);
        self.write(&mut out)?;
        Ok(out)
    }
    pub(crate) fn serialized_length(&self) -> Result<usize> {
        let escaped = |s: &str| {
            s.chars()
                .map(|c| match c {
                    '&' => 5,
                    '<' | '>' => 4,
                    '"' | '\'' => 6,
                    '\r' | '\n' => 5,
                    '\t' => 4,
                    c => c.len_utf8(),
                })
                .sum::<usize>()
        };
        let mut length = self.spelling.len() * 2 + 5;
        for (prefix, namespace) in self.namespaces.iter() {
            if prefix == "xml" {
                continue;
            }
            length = length
                .checked_add(
                    9 + prefix.len() + usize::from(!prefix.is_empty()) + escaped(namespace),
                )
                .ok_or_else(DavError::limit)?;
        }
        for (spelling, (_, value)) in self.attribute_spellings.iter().zip(&self.attributes) {
            length = length
                .checked_add(spelling.len() + escaped(value) + 4)
                .ok_or_else(DavError::limit)?;
        }
        for content in &self.content {
            length = length
                .checked_add(match content {
                    Content::Text(text) => escaped(text),
                    Content::Element(node) => node.serialized_length()?,
                    Content::Instruction(text) => text.len() + 4,
                })
                .ok_or_else(DavError::limit)?;
        }
        if length > XML_LIMIT {
            return Err(DavError::new(413, "DAV_LIMIT"));
        }
        Ok(length)
    }
    fn write(&self, out: &mut String) -> Result<()> {
        let tag = &self.spelling;
        out.push('<');
        out.push_str(tag);
        for (prefix, namespace) in self.namespaces.iter() {
            if prefix == "xml" {
                continue;
            }
            out.push_str(" xmlns");
            if !prefix.is_empty() {
                out.push(':');
                out.push_str(prefix);
            }
            out.push_str("=\"");
            escape_into(namespace, out);
            out.push('"');
        }
        for (spelling, (_, value)) in self.attribute_spellings.iter().zip(&self.attributes) {
            out.push(' ');
            out.push_str(spelling);
            out.push_str("=\"");
            escape_into(value, out);
            out.push('"');
        }
        out.push('>');
        for content in &self.content {
            match content {
                Content::Text(text) => escape_into(text, out),
                Content::Element(node) => node.write(out)?,
                Content::Instruction(text) => {
                    out.push_str("<?");
                    out.push_str(text);
                    out.push_str("?>");
                }
            }
        }
        out.push_str("</");
        out.push_str(tag);
        out.push('>');
        if out.len() > XML_LIMIT {
            return Err(DavError::new(413, "DAV_LIMIT"));
        }
        Ok(())
    }
}
pub fn escape_into(text: &str, output: &mut String) {
    for ch in text.chars() {
        match ch {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' => output.push_str("&quot;"),
            '\'' => output.push_str("&apos;"),
            '\r' => output.push_str("&#13;"),
            '\n' => output.push_str("&#10;"),
            '\t' => output.push_str("&#9;"),
            c => output.push(c),
        }
    }
}
pub fn escape(text: &str) -> String {
    let mut out = String::new();
    escape_into(text, &mut out);
    out
}
fn decoded(bytes: &[u8], attribute: bool) -> Result<String> {
    let raw = std::str::from_utf8(bytes).map_err(|_| DavError::invalid())?;
    let mut normalized = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            normalized.push(if attribute { ' ' } else { '\n' });
        } else {
            normalized.push(if attribute && matches!(ch, '\n' | '\t') {
                ' '
            } else {
                ch
            });
        }
    }
    let text = quick_xml::escape::unescape(&normalized)
        .map_err(|_| DavError::invalid())?
        .into_owned();
    if text.chars().any(|c| {
        !(matches!(c, '\t' | '\n' | '\r') || c >= ' ') || matches!(c, '\u{fffe}' | '\u{ffff}')
    }) {
        return Err(DavError::invalid());
    }
    Ok(text)
}
fn valid_name(value: &str) -> bool {
    let start = |c: char| matches!(c,'_'|'A'..='Z'|'a'..='z'|'\u{c0}'..='\u{d6}'|'\u{d8}'..='\u{f6}'|'\u{f8}'..='\u{2ff}'|'\u{370}'..='\u{37d}'|'\u{37f}'..='\u{1fff}'|'\u{200c}'..='\u{200d}'|'\u{2070}'..='\u{218f}'|'\u{2c00}'..='\u{2fef}'|'\u{3001}'..='\u{d7ff}'|'\u{f900}'..='\u{fdcf}'|'\u{fdf0}'..='\u{fffd}'|'\u{10000}'..='\u{effff}');
    let mut chars = value.chars();
    chars.next().is_some_and(start) && chars.all(|c| {
        start(c)
            || matches!(c,'-'|'.'|'0'..='9'|'\u{b7}'|'\u{300}'..='\u{36f}'|'\u{203f}'..='\u{2040}')
    })
}
fn name(
    raw: &[u8],
    namespaces: &BTreeMap<String, String>,
    attribute: bool,
    memory: &mut super::budget::Memory,
) -> Result<Name> {
    let raw = std::str::from_utf8(raw).map_err(|_| DavError::invalid())?;
    let (prefix, local) = raw.split_once(':').unwrap_or(("", raw));
    if !valid_name(local) || (!prefix.is_empty() && !valid_name(prefix)) {
        return Err(DavError::invalid());
    }
    memory.reserve(namespaces.get(prefix).map_or(0, String::len))?;
    let namespace = if prefix.is_empty() && attribute {
        String::new()
    } else {
        namespaces
            .get(prefix)
            .cloned()
            .ok_or_else(DavError::invalid)?
    };
    Ok(Name {
        namespace,
        local: local.into(),
    })
}
pub fn parse(bytes: &[u8]) -> Result<Node> {
    if bytes.len() > XML_LIMIT {
        return Err(DavError::new(413, "DAV_LIMIT"));
    }
    let source = std::str::from_utf8(bytes).map_err(|_| DavError::invalid())?;
    if source.chars().any(|c| {
        !(matches!(c, '\t' | '\r' | '\n') || c >= ' ') || matches!(c, '\u{fffe}' | '\u{ffff}')
    }) {
        return Err(DavError::invalid());
    }
    let mut memory = super::budget::Memory::acquire(
        bytes
            .len()
            .checked_mul(4)
            .and_then(|n| n.checked_add(32 * std::mem::size_of::<Node>() + 16384))
            .ok_or_else(DavError::limit)?,
    )?;
    let mut reader = Reader::from_reader(bytes);
    reader.config_mut().check_end_names = true;
    reader.config_mut().check_comments = true;
    let mut declared = false;
    let mut stack: Vec<(Node, std::sync::Arc<BTreeMap<String, String>>)> = Vec::new();
    let mut root = None;
    loop {
        match reader.read_event().map_err(|_| DavError::invalid())? {
            Event::Start(start) | Event::Empty(start) => {
                let empty =
                    bytes.get(reader.buffer_position().saturating_sub(2) as usize) == Some(&b'/');
                if stack.len() >= 32 {
                    return Err(DavError::new(413, "DAV_LIMIT"));
                }
                let _namespace_credit =
                    super::budget::Memory::acquire(stack.last().map_or(2048, |(_, ns)| {
                        ns.iter()
                            .map(|(key, value)| key.len() + value.len() + 512)
                            .sum()
                    }))?;
                let mut namespaces = stack.last().map_or_else(
                    || {
                        BTreeMap::from([
                            (String::new(), String::new()),
                            ("xml".into(), "http://www.w3.org/XML/1998/namespace".into()),
                        ])
                    },
                    |(_, ns)| ns.as_ref().clone(),
                );
                let mut attrs = Vec::new();
                let mut declarations = std::collections::BTreeSet::new();
                for attribute in start.attributes() {
                    let attribute = attribute.map_err(|_| DavError::invalid())?;
                    let key = std::str::from_utf8(attribute.key.as_ref())
                        .map_err(|_| DavError::invalid())?;
                    let value = decoded(attribute.value.as_ref(), true)?;
                    if key == "xmlns" {
                        if !declarations.insert(String::new()) {
                            return Err(DavError::invalid());
                        }
                        if matches!(value.as_str(), XML_NAMESPACE | XMLNS_NAMESPACE) {
                            return Err(DavError::invalid());
                        }
                        namespaces.insert(String::new(), value);
                    } else if let Some(prefix) = key.strip_prefix("xmlns:") {
                        memory.reserve(128 + prefix.len())?;
                        if !declarations.insert(prefix.to_owned()) {
                            return Err(DavError::invalid());
                        }
                        if !valid_name(prefix)
                            || prefix == "xmlns"
                            || value.is_empty()
                            || (prefix == "xml") != (value == XML_NAMESPACE)
                            || value == XMLNS_NAMESPACE
                        {
                            return Err(DavError::invalid());
                        }
                        memory.reserve(512 + prefix.len())?;
                        namespaces.insert(prefix.into(), value);
                    } else {
                        memory.reserve(2 * std::mem::size_of::<(String, String)>())?;
                        attrs.try_reserve_exact(1).map_err(|_| DavError::limit())?;
                        attrs.push((key.to_owned(), value));
                    }
                }
                let mut attributes = Vec::new();
                let mut attribute_spellings = Vec::new();
                for (spelling, value) in attrs {
                    memory.reserve(std::mem::size_of::<String>())?;
                    attribute_spellings
                        .try_reserve_exact(1)
                        .map_err(|_| DavError::limit())?;
                    let key = name(spelling.as_bytes(), &namespaces, true, &mut memory)?;
                    if attributes.iter().any(|(existing, _)| *existing == key) {
                        return Err(DavError::invalid());
                    }
                    memory.reserve(2 * std::mem::size_of::<(Name, String)>())?;
                    attributes
                        .try_reserve_exact(1)
                        .map_err(|_| DavError::limit())?;
                    attribute_spellings.push(spelling);
                    attributes.push((key, value));
                }
                let namespaces = std::sync::Arc::new(namespaces);
                let node = Node {
                    name: name(start.name().as_ref(), &namespaces, false, &mut memory)?,
                    spelling: std::str::from_utf8(start.name().as_ref())
                        .map_err(|_| DavError::invalid())?
                        .into(),
                    attributes,
                    attribute_spellings,
                    namespaces: namespaces.clone(),
                    _namespace_memory: _namespace_credit,
                    content: Vec::new(),
                    _memory: None,
                };
                if empty {
                    attach(node, &mut stack, &mut root, &mut memory)?;
                } else {
                    stack.push((node, namespaces));
                }
            }
            Event::End(_) => {
                let (node, _) = stack.pop().ok_or_else(DavError::invalid)?;
                attach(node, &mut stack, &mut root, &mut memory)?;
            }
            Event::Text(text) => {
                let text = decoded(text.as_ref(), false)?;
                if let Some((node, _)) = stack.last_mut() {
                    memory.reserve(2 * std::mem::size_of::<Content>())?;
                    node.content
                        .try_reserve_exact(1)
                        .map_err(|_| DavError::limit())?;
                    node.content.push(Content::Text(text));
                } else if !text.trim().is_empty() {
                    return Err(DavError::invalid());
                }
            }
            Event::CData(text) => {
                memory.reserve(2 * std::mem::size_of::<Content>())?;
                stack
                    .last_mut()
                    .ok_or_else(DavError::invalid)?
                    .0
                    .content
                    .try_reserve_exact(1)
                    .map_err(|_| DavError::limit())?;
                let text = std::str::from_utf8(text.as_ref())
                    .map_err(|_| DavError::invalid())?
                    .replace("\r\n", "\n")
                    .replace('\r', "\n");
                stack
                    .last_mut()
                    .ok_or_else(DavError::invalid)?
                    .0
                    .content
                    .push(Content::Text(text));
            }
            Event::GeneralRef(reference) => {
                memory.reserve(2 * std::mem::size_of::<Content>())?;
                stack
                    .last_mut()
                    .ok_or_else(DavError::invalid)?
                    .0
                    .content
                    .try_reserve_exact(1)
                    .map_err(|_| DavError::limit())?;
                let raw = format!(
                    "&{};",
                    std::str::from_utf8(reference.as_ref()).map_err(|_| DavError::invalid())?
                );
                let text = decoded(raw.as_bytes(), false)?;
                stack
                    .last_mut()
                    .ok_or_else(DavError::invalid)?
                    .0
                    .content
                    .push(Content::Text(text));
            }
            Event::DocType(_) => return Err(DavError::invalid()),
            Event::PI(instruction) => {
                let text =
                    std::str::from_utf8(instruction.as_ref()).map_err(|_| DavError::invalid())?;
                let target = text
                    .split_whitespace()
                    .next()
                    .ok_or_else(DavError::invalid)?;
                if target.eq_ignore_ascii_case("xml") || !target.split(':').all(valid_name) {
                    return Err(DavError::invalid());
                }
                if let Some((node, _)) = stack.last_mut() {
                    memory.reserve(2 * std::mem::size_of::<Content>())?;
                    node.content
                        .try_reserve_exact(1)
                        .map_err(|_| DavError::limit())?;
                    node.content.push(Content::Instruction(text.into()));
                }
            }
            Event::Decl(decl) => {
                if declared
                    || !source.trim_start_matches('\u{feff}').starts_with("<?xml")
                    || root.is_some()
                    || !stack.is_empty()
                    || decl.version().map_err(|_| DavError::invalid())?.as_ref() != b"1.0"
                    || decl.encoding().is_some_and(|v| {
                        v.is_err() || v.is_ok_and(|s| !s.eq_ignore_ascii_case(b"utf-8"))
                    })
                {
                    return Err(DavError::invalid());
                }
                declared = true;
            }
            Event::Comment(_) => (),
            Event::Eof => break,
        }
    }
    if !stack.is_empty() {
        return Err(DavError::invalid());
    }
    let mut root = root.ok_or_else(DavError::invalid)?;
    root._memory = Some(memory);
    Ok(root)
}
fn attach(
    node: Node,
    stack: &mut [(Node, std::sync::Arc<BTreeMap<String, String>>)],
    root: &mut Option<Node>,
    memory: &mut super::budget::Memory,
) -> Result<()> {
    if let Some((parent, _)) = stack.last_mut() {
        memory.reserve(2 * std::mem::size_of::<Content>())?;
        parent
            .content
            .try_reserve_exact(1)
            .map_err(|_| DavError::limit())?;
        parent.content.push(Content::Element(node));
    } else if root.replace(node).is_some() {
        return Err(DavError::invalid());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn xml_preserves_xml_namespace_character_references_and_processing_instructions() -> Result<()>
    {
        let xml=br#"<p:note xmlns:p="urn:example" xmlns:xs="urn:types" xml:lang="en" a="&#9;&#10;&#13;" type="xs:int">&#13;<?client data?><p:n/></p:note>"#;
        let parsed = parse(xml)?;
        let serialized = parsed.serialize()?;
        let reparsed = parse(serialized.as_bytes())?;
        assert_eq!(reparsed.attributes[0].0.namespace, XML_NAMESPACE);
        assert_eq!(reparsed.attributes[1].1, "\t\n\r");
        assert_eq!(reparsed.text(), "\r");
        assert!(serialized.contains("<?client data?>"));
        assert!(serialized.contains("xmlns:xs=\"urn:types\""));
        assert!(serialized.contains("type=\"xs:int\""));
        for invalid in [
            "<1name/>",
            "<a xmlns:p=\"http://www.w3.org/XML/1998/namespace\"/>",
            "<a><![CDATA[\0]]></a>",
            "<?xml version=\"1.0\"?><?xml version=\"1.0\"?><a/>",
        ] {
            assert!(parse(invalid.as_bytes()).is_err());
        }
        Ok(())
    }
    #[test]
    fn xml_preserves_expanded_names_and_rejects_entities_and_excess_depth() -> Result<()> {
        let xml = br#"<d:prop xmlns:d="DAV:" xmlns:q="urn:custom"><q:note q:id="a&amp;b">a&lt;<q:n/>tail</q:note></d:prop>"#;
        let parsed = parse(xml)?;
        let serialized = parsed.serialize()?;
        let reparsed = parse(serialized.as_bytes())?;
        assert_eq!(
            reparsed.elements().next().unwrap().name.namespace,
            "urn:custom"
        );
        assert_eq!(reparsed.elements().next().unwrap().attributes[0].1, "a&b");
        for xml in [
            b"<!DOCTYPE a [<!ENTITY x SYSTEM 'file:///etc/passwd'>]><a>&x;</a>".as_slice(),
            b"<a>&unknown;</a>",
            b"<a/><b/>",
        ] {
            assert!(parse(xml).is_err());
        }
        assert!(parse(format!("{}{}", "<a>".repeat(33), "</a>".repeat(33)).as_bytes()).is_err());
        Ok(())
    }
}
