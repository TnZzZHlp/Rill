use super::{
    limits::Cancellation,
    model::{DavError, Result},
    namespace,
    path::DavPath,
    properties,
    registry::StorageHandle,
    request_model::{DavRequest, single},
    xml::{self, Name},
};
use nix::{
    fcntl::{OFlag, OpenHow, ResolveFlag, openat2},
    sys::stat::Mode,
};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{Read, Write},
    sync::Arc,
    time::Instant,
};

struct MethodStorage {
    root: Arc<namespace::NamespaceSession>,
}
impl std::ops::Deref for MethodStorage {
    type Target = StorageHandle;
    fn deref(&self) -> &Self::Target {
        self.root.handle()
    }
}

pub const ALLOW: &str =
    "OPTIONS, PROPFIND, PROPPATCH, MKCOL, GET, HEAD, PUT, DELETE, COPY, MOVE, LOCK, UNLOCK";
pub enum Message {
    Header(u16, Vec<(&'static str, String)>),
    Data(super::budget::Buffer),
    End,
    Error(DavError),
}
fn send(sender: &tokio::sync::mpsc::Sender<Message>, message: Message) -> Result<()> {
    sender
        .blocking_send(message)
        .map_err(|_| DavError::new(499, "DAV_CANCELLED"))
}
fn data(sender: &tokio::sync::mpsc::Sender<Message>, text: &str) -> Result<()> {
    for bytes in text.as_bytes().chunks(super::limits::BLOCK) {
        send(
            sender,
            Message::Data(super::budget::Buffer::copy_from(bytes)?),
        )?;
    }
    Ok(())
}
fn response(
    sender: &tokio::sync::mpsc::Sender<Message>,
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: &str,
) -> Result<()> {
    let mut headers = headers;
    headers.push(("content-length", body.len().to_string()));
    if !body.is_empty() {
        headers.push(("content-type", "application/xml; charset=utf-8".into()));
    }
    send(sender, Message::Header(status, headers))?;
    data(sender, body)?;
    send(sender, Message::End)
}
fn multistatus_start(sender: &tokio::sync::mpsc::Sender<Message>) -> Result<()> {
    send(
        sender,
        Message::Header(
            207,
            vec![("content-type", "application/xml; charset=utf-8".into())],
        ),
    )?;
    data(
        sender,
        "<?xml version=\"1.0\" encoding=\"utf-8\"?><d:multistatus xmlns:d=\"DAV:\">",
    )
}
fn multistatus_end(sender: &tokio::sync::mpsc::Sender<Message>) -> Result<()> {
    data(sender, "</d:multistatus>")?;
    send(sender, Message::End)
}
fn status(code: u16) -> String {
    format!(
        "HTTP/1.1 {code} {}",
        http::StatusCode::from_u16(code)
            .ok()
            .and_then(|s| s.canonical_reason())
            .unwrap_or("Error")
    )
}
fn property_name(name: &Name) -> String {
    if name.namespace.is_empty() {
        return format!("<{} xmlns=\"\"/>", name.local);
    }
    if name.namespace == xml::XML_NAMESPACE {
        return format!("<xml:{}/>", name.local);
    }
    format!(
        "<p:{} xmlns:p=\"{}\"/>",
        name.local,
        xml::escape(&name.namespace)
    )
}
fn lock_xml(lock: &super::locks::Lock, href: &str) -> String {
    format!(
        "<d:activelock><d:locktype><d:write/></d:locktype><d:lockscope><d:exclusive/></d:lockscope><d:depth>{}</d:depth>{}<d:timeout>Second-{}</d:timeout><d:locktoken><d:href>{}</d:href></d:locktoken><d:lockroot><d:href>{}</d:href></d:lockroot></d:activelock>",
        if lock.infinity { "infinity" } else { "0" },
        lock.owner,
        lock.seconds(),
        xml::escape(&lock.token),
        xml::escape(href)
    )
}
fn locks_property(
    storage: &MethodStorage,
    path: &DavPath,
    request: &mut DavRequest,
) -> Result<String> {
    let locks = storage
        .locks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut out = String::from("<d:lockdiscovery>");
    for lock in locks.active().filter(|lock| lock.applies(path)) {
        let href = request.authorization.href(&request.path, &lock.path)?;
        out.push_str(&lock_xml(lock, &href));
    }
    out.push_str("</d:lockdiscovery>");
    Ok(out)
}
fn live(
    storage: &MethodStorage,
    path: &DavPath,
    request: &mut DavRequest,
) -> Result<BTreeMap<Name, String>> {
    let metadata = namespace::open(&storage.root, path)?.metadata()?;
    let mut properties = BTreeMap::new();
    properties.insert(
        Name::dav("displayname"),
        format!(
            "<d:displayname>{}</d:displayname>",
            xml::escape(path.name())
        ),
    );
    properties.insert(
        Name::dav("resourcetype"),
        if metadata.is_dir() {
            "<d:resourcetype><d:collection/></d:resourcetype>"
        } else {
            "<d:resourcetype/>"
        }
        .into(),
    );
    properties.insert(
        Name::dav("getcontentlength"),
        format!(
            "<d:getcontentlength>{}</d:getcontentlength>",
            if metadata.is_file() {
                metadata.len()
            } else {
                0
            }
        ),
    );
    properties.insert(
        Name::dav("getcontenttype"),
        format!(
            "<d:getcontenttype>{}</d:getcontenttype>",
            if metadata.is_dir() {
                "httpd/unix-directory"
            } else {
                crate::static_files::response::mime_type(path.relative())
            }
        ),
    );
    properties.insert(
        Name::dav("getetag"),
        format!(
            "<d:getetag>{}</d:getetag>",
            xml::escape(&namespace::etag(&metadata))
        ),
    );
    if let Ok(modified) = metadata.modified() {
        properties.insert(
            Name::dav("getlastmodified"),
            format!(
                "<d:getlastmodified>{}</d:getlastmodified>",
                httpdate::fmt_http_date(modified)
            ),
        );
    }
    properties.insert(Name::dav("supportedlock"),"<d:supportedlock><d:lockentry><d:lockscope><d:exclusive/></d:lockscope><d:locktype><d:write/></d:locktype></d:lockentry></d:supportedlock>".into());
    properties.insert(
        Name::dav("lockdiscovery"),
        locks_property(storage, path, request)?,
    );
    Ok(properties)
}
struct Selection {
    node: Option<xml::Node>,
    all: bool,
    names_only: bool,
}
impl Selection {
    fn names(&self) -> impl Iterator<Item = &Name> {
        self.node
            .iter()
            .flat_map(xml::Node::elements)
            .filter(|node| node.is("prop") || node.is("include"))
            .flat_map(xml::Node::elements)
            .map(|node| &node.name)
    }
}
fn selection(body: &[u8]) -> Result<Selection> {
    if body.is_empty() {
        return Ok(Selection {
            node: None,
            all: true,
            names_only: false,
        });
    }
    let node = xml::parse(body)?;
    if !node.is("propfind") {
        return Err(DavError::invalid());
    }
    let mut all = false;
    let mut names_only = false;
    let mut chosen = false;
    for child in node.elements() {
        if child.is("include") {
            continue;
        }
        if chosen {
            return Err(DavError::invalid());
        }
        chosen = true;
        if child.is("allprop") {
            all = true;
        } else if child.is("propname") {
            all = true;
            names_only = true;
        } else if !child.is("prop") {
            return Err(DavError::invalid());
        }
    }
    if !chosen {
        return Err(DavError::invalid());
    }
    Ok(Selection {
        node: Some(node),
        all,
        names_only,
    })
}
#[derive(Default)]
struct PropfindBody {
    blocks: Vec<super::budget::Buffer>,
    index: super::budget::Memory,
}
impl PropfindBody {
    fn text(&mut self, mut text: &str) -> Result<()> {
        while !text.is_empty() {
            if self
                .blocks
                .last()
                .is_none_or(|block| block.as_ref().len() == super::limits::BLOCK)
            {
                self.index
                    .reserve(2 * std::mem::size_of::<super::budget::Buffer>())?;
                self.blocks
                    .try_reserve_exact(1)
                    .map_err(|_| DavError::limit())?;
                self.blocks
                    .push(super::budget::Buffer::with_capacity(super::limits::BLOCK)?);
            }
            let block = self.blocks.last_mut().ok_or_else(DavError::invalid)?;
            let mut amount = text.len().min(super::limits::BLOCK - block.as_ref().len());
            while !text.is_char_boundary(amount) {
                amount -= 1;
            }
            if amount == 0 {
                self.index
                    .reserve(2 * std::mem::size_of::<super::budget::Buffer>())?;
                self.blocks
                    .try_reserve_exact(1)
                    .map_err(|_| DavError::limit())?;
                self.blocks
                    .push(super::budget::Buffer::with_capacity(super::limits::BLOCK)?);
                continue;
            }
            block.extend(&text.as_bytes()[..amount], super::limits::BLOCK)?;
            text = &text[amount..];
        }
        Ok(())
    }
    fn escaped(&mut self, text: &str) -> Result<()> {
        let mut start = 0;
        for (index, ch) in text.char_indices() {
            let replacement = match ch {
                '&' => "&amp;",
                '<' => "&lt;",
                '>' => "&gt;",
                '"' => "&quot;",
                '\'' => "&apos;",
                '\r' => "&#13;",
                '\n' => "&#10;",
                '\t' => "&#9;",
                _ => continue,
            };
            self.text(&text[start..index])?;
            self.text(replacement)?;
            start = index + ch.len_utf8();
        }
        self.text(&text[start..])
    }
    fn name(&mut self, name: &Name) -> Result<()> {
        if name.namespace.is_empty() {
            self.text("<")?;
            self.text(&name.local)?;
            self.text(" xmlns=\"\"/>")
        } else if name.namespace == xml::XML_NAMESPACE {
            self.text("<xml:")?;
            self.text(&name.local)?;
            self.text("/>")
        } else {
            self.text("<p:")?;
            self.text(&name.local)?;
            self.text(" xmlns:p=\"")?;
            self.escaped(&name.namespace)?;
            self.text("\"/>")
        }
    }
}
fn live_credit(
    storage: &MethodStorage,
    path: &DavPath,
    request: &DavRequest,
) -> Result<super::budget::Memory> {
    let locks = storage
        .locks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut bytes = 16384_usize.saturating_add(12 * path.key().len());
    for lock in locks.active().filter(|lock| lock.applies(path)) {
        let maximum = lock
            .owner
            .len()
            .saturating_add(6 * lock.token.len())
            .saturating_add(
                6 * request
                    .authorization
                    .href_capacity(&request.path, &lock.path),
            )
            .saturating_add(1024);
        bytes = bytes.saturating_add(maximum.saturating_mul(4));
    }
    super::budget::Memory::acquire(bytes)
}
fn propfind(
    storage: &MethodStorage,
    request: &mut DavRequest,
    body: &[u8],
    sender: &tokio::sync::mpsc::Sender<Message>,
    cancellation: &Cancellation,
    deadline: Instant,
) -> Result<()> {
    request.check(storage)?;
    let selection = selection(body)?;
    let paths = namespace::walk(&storage.root, &request.path, request.depth)?;
    let mut hrefs = Vec::new();
    let mut href_memory = super::budget::Memory::default();
    for path in &paths {
        href_memory.reserve(
            request.authorization.href_capacity(&request.path, path)
                + 2 * std::mem::size_of::<String>(),
        )?;
        hrefs.try_reserve_exact(1).map_err(|_| DavError::limit())?;
        let mut href = request.authorization.href(&request.path, path)?;
        if namespace::open(&storage.root, path)?.metadata()?.is_dir() && !href.ends_with('/') {
            href.push('/');
        }
        hrefs.push(href);
    }
    let mut output = PropfindBody::default();
    output.text("<?xml version=\"1.0\" encoding=\"utf-8\"?><d:multistatus xmlns:d=\"DAV:\">")?;
    for (path, href) in paths.iter().zip(hrefs) {
        cancellation.check(deadline)?;
        let _live_credit = live_credit(storage, path, request)?;
        let properties = live(storage, path, request)?;
        let _property_credit = super::budget::Memory::acquire(xml::XML_LIMIT)?;
        let dead = storage.properties.load(path)?;
        let names = || {
            selection
                .names()
                .chain(properties.keys().filter(|name| {
                    selection.all && !selection.names().any(|selected| selected == *name)
                }))
                .chain(dead.keys().filter(|name| {
                    selection.all
                        && !properties.contains_key(*name)
                        && !selection.names().any(|selected| selected == *name)
                }))
        };
        output.text("<d:response><d:href>")?;
        output.escaped(&href)?;
        output.text("</d:href>")?;
        for present in [true, false] {
            let mut opened = false;
            for name in names() {
                if (properties.contains_key(name) || dead.contains_key(name)) != present {
                    continue;
                }
                if !opened {
                    output.text("<d:propstat><d:prop>")?;
                    opened = true;
                }
                if !present || selection.names_only {
                    output.name(name)?;
                } else if let Some(node) = dead.get(name) {
                    let _serialized = super::budget::Memory::acquire(node.serialized_length()?)?;
                    output.text(&node.serialize()?)?;
                } else if let Some(value) = properties.get(name) {
                    output.text(value)?;
                }
            }
            if opened {
                output.text(if present {
                    "</d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>"
                } else {
                    "</d:prop><d:status>HTTP/1.1 404 Not Found</d:status></d:propstat>"
                })?;
            }
        }
        output.text("</d:response>")?;
    }
    output.text("</d:multistatus>")?;
    drop(selection);
    send(
        sender,
        Message::Header(
            207,
            vec![("content-type", "application/xml; charset=utf-8".into())],
        ),
    )?;
    for block in output.blocks {
        send(sender, Message::Data(block))?;
    }
    send(sender, Message::End)
}
fn proppatch(
    storage: &MethodStorage,
    request: &mut DavRequest,
    body: &[u8],
    sender: &tokio::sync::mpsc::Sender<Message>,
    cancellation: &Cancellation,
    deadline: Instant,
) -> Result<()> {
    let submitted = request.check(storage)?;
    namespace::writable_metadata(&storage.root, &request.path)?;
    storage
        .locks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .check(&request.path, false, false, &submitted)?;
    let node = xml::parse(body)?;
    if !node.is("propertyupdate") {
        return Err(DavError::invalid());
    }
    let mut changes = Vec::new();
    for op in node.elements() {
        if !op.is("set") && !op.is("remove") {
            return Err(DavError::invalid());
        }
        let children: Vec<_> = op.elements().collect();
        if children.len() != 1 || !children[0].is("prop") {
            return Err(DavError::invalid());
        }
        for node in children[0].elements() {
            if changes.len() >= 64 {
                return Err(DavError::new(413, "DAV_LIMIT"));
            }
            changes.push((op.is("set"), node));
        }
    }
    if changes.is_empty() {
        return Err(DavError::invalid());
    }
    let protected = changes.iter().any(|(_, node)| properties::live(&node.name));
    if !protected {
        let mut properties = storage.properties.load(&request.path)?;
        for (set, node) in &changes {
            if *set {
                properties.insert(node.name.clone(), node.duplicate()?);
            } else {
                properties.remove(&node.name);
            }
        }
        storage.properties.store(&request.path, &properties, || {
            let submitted = request.check(storage)?;
            namespace::writable_metadata(&storage.root, &request.path)?;
            storage
                .locks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .check(&request.path, false, false, &submitted)?;
            cancellation.admit_commit(deadline)
        })?;
    }
    let href = request.authorization.href(&request.path, &request.path)?;
    let mut output = format!(
        "<?xml version=\"1.0\"?><d:multistatus xmlns:d=\"DAV:\"><d:response><d:href>{}</d:href>",
        xml::escape(&href)
    );
    for (_, node) in changes {
        let code = if !protected {
            200
        } else if properties::live(&node.name) {
            403
        } else {
            424
        };
        output.push_str(&format!(
            "<d:propstat><d:prop>{}</d:prop><d:status>{}</d:status></d:propstat>",
            property_name(&node.name),
            status(code)
        ));
    }
    output.push_str("</d:response></d:multistatus>");
    response(sender, 207, Vec::new(), &output)
}
fn remap(source: &DavPath, destination: &DavPath, path: &DavPath) -> Result<DavPath> {
    let suffix = path
        .key()
        .strip_prefix(source.key())
        .ok_or_else(DavError::invalid)?
        .trim_start_matches('/');
    let mut target = destination.clone();
    for component in suffix.split('/').filter(|s| !s.is_empty()) {
        target = target.child(component)?;
    }
    Ok(target)
}
fn delete_one(
    storage: &MethodStorage,
    path: &DavPath,
    submitted: &[(DavPath, String)],
    clear_locks: bool,
    cancellation: &Cancellation,
    deadline: Instant,
) -> Result<()> {
    storage
        .locks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .check(path, false, true, submitted)?;
    let parent = namespace::parent(&storage.root, path)?;
    let directory = namespace::open(&storage.root, path)?.metadata()?.is_dir();
    namespace::revalidate(&storage.root, path, &parent)?;
    cancellation.check(deadline)?;
    namespace::remove_entry(&storage.root, &parent, path.name(), directory)?;
    storage
        .properties
        .delete(path, || cancellation.check(deadline))?;
    if clear_locks {
        storage
            .locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(path);
    }
    Ok(())
}
fn batch_errors(
    request: &mut DavRequest,
    errors: &[(DavPath, DavError)],
    sender: &tokio::sync::mpsc::Sender<Message>,
) -> Result<()> {
    multistatus_start(sender)?;
    for (path, error) in errors {
        let href = if let (Some(target), Some(external)) =
            (&request.destination, &request.destination_external)
        {
            let mapped = remap(&request.path, target, path)?;
            request.authorization.href_from(external, target, &mapped)?
        } else {
            request.authorization.href(&request.path, path)?
        };
        data(
            sender,
            &format!(
                "<d:response><d:href>{}</d:href><d:status>{}</d:status></d:response>",
                xml::escape(&href),
                status(error.status)
            ),
        )?;
    }
    multistatus_end(sender)
}
fn delete(
    storage: &MethodStorage,
    request: &mut DavRequest,
    sender: &tokio::sync::mpsc::Sender<Message>,
    cancellation: &Cancellation,
    deadline: Instant,
) -> Result<()> {
    if request.path.is_root() {
        return Err(DavError::new(403, "DAV_ROOT"));
    }
    if request.depth != usize::MAX {
        return Err(DavError::invalid());
    }
    let submitted = request.check(storage)?;
    storage
        .locks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .check(&request.path, false, true, &submitted)?;
    let paths = namespace::walk(&storage.root, &request.path, usize::MAX)?;
    for path in &paths {
        request.authorization.href(&request.path, path)?;
    }
    let mut errors = Vec::new();
    for path in paths.iter().rev() {
        cancellation.check(deadline)?;
        if errors.iter().any(|(failed, _)| path.contains(failed)) {
            continue;
        }
        if let Err(error) = delete_one(storage, path, &submitted, true, cancellation, deadline) {
            if paths.len() == 1 {
                return Err(error);
            }
            errors.push((path.clone(), error));
        }
    }
    if errors.is_empty() {
        response(sender, 204, Vec::new(), "")
    } else {
        batch_errors(request, &errors, sender)
    }
}
fn copy_file(
    storage: &MethodStorage,
    source: &DavPath,
    target: &DavPath,
    cancellation: &Cancellation,
    deadline: Instant,
) -> Result<()> {
    let mut source_file = namespace::readable(&storage.root, source)?;
    let parent = namespace::parent(&storage.root, target)?;
    if namespace::metadata(&storage.root, target)?.is_some() {
        return Err(DavError::new(409, "DAV_NAMESPACE_CHANGED"));
    }
    namespace::reserve_create(&storage.root, target.name())?;
    cancellation.check(deadline)?;
    let mut target_file = File::from(openat2(
        &parent,
        target.name(),
        OpenHow::new()
            .flags(
                OFlag::O_WRONLY
                    | OFlag::O_CREAT
                    | OFlag::O_EXCL
                    | OFlag::O_NOFOLLOW
                    | OFlag::O_CLOEXEC,
            )
            .mode(Mode::from_bits_truncate(0o600))
            .resolve(
                ResolveFlag::RESOLVE_BENEATH
                    | ResolveFlag::RESOLVE_NO_MAGICLINKS
                    | ResolveFlag::RESOLVE_NO_SYMLINKS,
            ),
    )?);
    namespace::created_file(&storage.root, &parent, target.name(), &target_file)?;
    let mut buffer = vec![0_u8; super::limits::BLOCK];
    loop {
        cancellation.check(deadline)?;
        let read = source_file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        cancellation.check(deadline)?;
        target_file.write_all(&buffer[..read])?;
    }
    target_file.sync_all()?;
    parent.sync_all()?;
    Ok(())
}
fn copy_move(
    storage: &MethodStorage,
    request: &mut DavRequest,
    sender: &tokio::sync::mpsc::Sender<Message>,
    cancellation: &Cancellation,
    deadline: Instant,
) -> Result<()> {
    let moving = request.method.as_str() == "MOVE";
    if (moving && request.depth != usize::MAX) || (!moving && request.depth == 1) {
        return Err(DavError::invalid());
    }
    if moving && request.path.is_root() {
        return Err(DavError::new(403, "DAV_ROOT"));
    }
    let target = request.destination.clone().ok_or_else(DavError::invalid)?;
    if target.is_root() || request.path.contains(&target) || target.contains(&request.path) {
        return Err(DavError::new(403, "DAV_DESTINATION_RELATION"));
    }
    let submitted = request.check(storage)?;
    let paths = namespace::walk(
        &storage.root,
        &request.path,
        if moving { usize::MAX } else { request.depth },
    )?;
    let external = request
        .destination_external
        .clone()
        .ok_or_else(DavError::invalid)?;
    for path in &paths {
        request.authorization.href(&request.path, path)?;
        let mapped = remap(&request.path, &target, path)?;
        request
            .authorization
            .href_from(&external, &target, &mapped)?;
    }
    if !moving {
        for path in &paths {
            namespace::readable_or_directory(&storage.root, path)?;
        }
    }
    let exists = namespace::metadata(&storage.root, &target)?.is_some();
    if exists && !request.overwrite {
        return Err(DavError::new(412, "DAV_OVERWRITE"));
    }
    {
        let locks = storage
            .locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        locks.check(&target, exists, true, &submitted)?;
        if moving {
            locks.check(&request.path, true, true, &submitted)?;
        }
    }
    let target_parent = namespace::parent(&storage.root, &target)?;
    if moving {
        let source = namespace::open(&storage.root, &request.path)?;
        let source_parent = namespace::parent(&storage.root, &request.path)?;
        if storage.root.mount_id(&source_parent)? != storage.root.mount_id(&target_parent)?
            || storage.root.mount_id(&source)? != storage.root.mount_id(&source_parent)?
        {
            return Err(DavError::new(409, "DAV_CROSS_DEVICE"));
        }
    }
    if exists {
        let previous = namespace::walk(&storage.root, &target, usize::MAX)?;
        for path in &previous {
            request.authorization.href_from(&external, &target, path)?;
        }
        storage.root.validate_tags()?;
        for path in previous.iter().rev() {
            cancellation.check(deadline)?;
            delete_one(storage, path, &submitted, false, cancellation, deadline)?;
        }
    }
    if moving {
        let parent = namespace::parent(&storage.root, &request.path)?;
        namespace::revalidate(&storage.root, &request.path, &parent)?;
        namespace::revalidate(&storage.root, &target, &target_parent)?;
        let source = namespace::open(&storage.root, &request.path)?;
        let rename = namespace::prepare_rename(
            &storage.root,
            &parent,
            request.path.name(),
            &target_parent,
            target.name(),
            &source,
        )?;
        cancellation.check(deadline)?;
        rename.commit()?;
        parent
            .sync_all()
            .map_err(|_| DavError::new(500, "DAV_COMMIT_DURABILITY_UNKNOWN"))?;
        target_parent
            .sync_all()
            .map_err(|_| DavError::new(500, "DAV_COMMIT_DURABILITY_UNKNOWN"))?;
        storage
            .locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&request.path);
        let mut errors = Vec::new();
        for path in &paths {
            let result = (|| {
                cancellation.check(deadline)?;
                let destination = remap(&request.path, &target, path)?;
                let properties = storage.properties.load(path)?;
                storage
                    .properties
                    .store(&destination, &properties, || cancellation.check(deadline))?;
                storage
                    .properties
                    .delete(path, || cancellation.check(deadline))
            })();
            if let Err(error) = result {
                errors.push((path.clone(), error));
            }
        }
        return if errors.is_empty() {
            response(sender, if exists { 204 } else { 201 }, Vec::new(), "")
        } else {
            batch_errors(request, &errors, sender)
        };
    }
    let mut errors: Vec<(DavPath, DavError)> = Vec::new();
    for path in &paths {
        cancellation.check(deadline)?;
        if errors.iter().any(|(failed, _)| failed.contains(path)) {
            continue;
        }
        let destination = remap(&request.path, &target, path)?;
        let result = (|| {
            if namespace::open(&storage.root, path)?.metadata()?.is_dir() {
                let parent = namespace::parent(&storage.root, &destination)?;
                if namespace::metadata(&storage.root, &destination)?.is_some() {
                    return Err(DavError::new(409, "DAV_NAMESPACE_CHANGED"));
                }
                cancellation.check(deadline)?;
                namespace::mkdir(&storage.root, &parent, destination.name())?;
            } else {
                copy_file(storage, path, &destination, cancellation, deadline)?;
            }
            let properties = storage.properties.load(path)?;
            storage
                .properties
                .store(&destination, &properties, || cancellation.check(deadline))?;
            Ok(())
        })();
        if let Err(error) = result {
            if *path == request.path {
                return Err(error);
            }
            errors.push((path.clone(), error));
        }
    }
    if errors.is_empty() {
        response(sender, if exists { 204 } else { 201 }, Vec::new(), "")
    } else {
        batch_errors(request, &errors, sender)
    }
}
fn lock(
    storage: &MethodStorage,
    request: &mut DavRequest,
    body: &[u8],
    sender: &tokio::sync::mpsc::Sender<Message>,
    cancellation: &Cancellation,
    deadline: Instant,
) -> Result<()> {
    if request.depth == 1 {
        return Err(DavError::invalid());
    }
    let submitted = request.check(storage)?;
    let exists = namespace::metadata(&storage.root, &request.path)?.is_some();
    if exists {
        namespace::writable_metadata(&storage.root, &request.path)?;
    }
    cancellation.check(deadline)?;
    let token = if body.is_empty() {
        storage
            .locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .refresh(&request.path, &submitted, request.timeout)?
    } else {
        let node = xml::parse(body)?;
        if !node.is("lockinfo") {
            return Err(DavError::invalid());
        }
        let mut scope = false;
        let mut kind = false;
        let mut owner = String::new();
        for child in node.elements() {
            if child.is("lockscope") && !scope {
                let children: Vec<_> = child.elements().collect();
                if children.len() != 1 || !children[0].is("exclusive") {
                    return Err(DavError::new(501, "DAV_LOCK_SCOPE"));
                }
                scope = true;
            } else if child.is("locktype") && !kind {
                let children: Vec<_> = child.elements().collect();
                if children.len() != 1 || !children[0].is("write") {
                    return Err(DavError::invalid());
                }
                kind = true;
            } else if child.is("owner") && owner.is_empty() {
                owner = child.serialize()?;
            } else {
                return Err(DavError::invalid());
            }
        }
        if !scope || !kind {
            return Err(DavError::invalid());
        }
        storage
            .locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .check(&request.path, false, !exists, &submitted)?;
        cancellation.check(deadline)?;
        let token = storage
            .locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .create(
                request.path.clone(),
                request.depth == usize::MAX,
                owner,
                request.timeout,
            )?;
        if !exists {
            let created = (|| {
                let parent = namespace::parent(&storage.root, &request.path)?;
                namespace::reserve_create(&storage.root, request.path.name())?;
                cancellation.check(deadline)?;
                let file = File::from(openat2(
                    &parent,
                    request.path.name(),
                    OpenHow::new()
                        .flags(
                            OFlag::O_WRONLY
                                | OFlag::O_CREAT
                                | OFlag::O_EXCL
                                | OFlag::O_NOFOLLOW
                                | OFlag::O_CLOEXEC,
                        )
                        .mode(Mode::from_bits_truncate(0o600))
                        .resolve(
                            ResolveFlag::RESOLVE_BENEATH
                                | ResolveFlag::RESOLVE_NO_MAGICLINKS
                                | ResolveFlag::RESOLVE_NO_SYMLINKS,
                        ),
                )?);
                namespace::created_file(&storage.root, &parent, request.path.name(), &file)?;
                file.sync_all()?;
                parent.sync_all()?;
                Ok(())
            })();
            if let Err(error) = created {
                let _ = storage
                    .locks
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .unlock(&request.path, &token);
                return Err(error);
            }
        }
        token
    };
    let path = request.path.clone();
    let discovery = locks_property(storage, &path, request)?;
    response(
        sender,
        if exists { 200 } else { 201 },
        vec![("lock-token", format!("<{token}>"))],
        &format!("<?xml version=\"1.0\"?><d:prop xmlns:d=\"DAV:\">{discovery}</d:prop>"),
    )
}
pub fn execute(
    mut request: DavRequest,
    body: &[u8],
    sender: &tokio::sync::mpsc::Sender<Message>,
    cancellation: &Cancellation,
    deadline: Instant,
) -> Result<()> {
    cancellation.check(deadline)?;
    let storage = MethodStorage {
        root: request.namespace.clone(),
    };
    super::registry::registry().collect_expired();
    let _response_credit = if request.method.as_str() == "PROPFIND" {
        None
    } else {
        Some(super::budget::Memory::acquire(4 * xml::XML_LIMIT)?)
    };
    storage
        .locks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .expire();
    match request.method.as_str() {
        "OPTIONS" => {
            request.check(&storage)?;
            response(
                sender,
                200,
                vec![
                    ("dav", "1, 2".into()),
                    ("allow", ALLOW.into()),
                    ("ms-author-via", "DAV".into()),
                ],
                "",
            )
        }
        "PROPFIND" => propfind(&storage, &mut request, body, sender, cancellation, deadline),
        "PROPPATCH" => proppatch(&storage, &mut request, body, sender, cancellation, deadline),
        "MKCOL" => {
            if !body.is_empty() {
                return Err(DavError::new(415, "DAV_MKCOL_BODY"));
            }
            let submitted = request.check(&storage)?;
            if namespace::metadata(&storage.root, &request.path)?.is_some() {
                return Err(DavError::new(405, "DAV_EXISTS"));
            }
            storage
                .locks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .check(&request.path, false, true, &submitted)?;
            let parent = namespace::parent(&storage.root, &request.path).map_err(|error| {
                if error.status == 404 {
                    DavError::new(409, "DAV_PARENT")
                } else {
                    error
                }
            })?;
            cancellation.check(deadline)?;
            namespace::mkdir(&storage.root, &parent, request.path.name())?;
            parent
                .sync_all()
                .map_err(|_| DavError::new(500, "DAV_COMMIT_DURABILITY_UNKNOWN"))?;
            response(sender, 201, Vec::new(), "")
        }
        "DELETE" => delete(&storage, &mut request, sender, cancellation, deadline),
        "COPY" | "MOVE" => copy_move(&storage, &mut request, sender, cancellation, deadline),
        "LOCK" => lock(&storage, &mut request, body, sender, cancellation, deadline),
        "UNLOCK" => {
            request.check(&storage)?;
            let token = single(&request.headers, "lock-token")?
                .and_then(|s| s.strip_prefix('<'))
                .and_then(|s| s.strip_suffix('>'))
                .filter(|s| !s.contains(['<', '>', ' ', '\t']))
                .ok_or_else(DavError::invalid)?;
            cancellation.check(deadline)?;
            storage
                .locks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .unlock(&request.path, token)?;
            response(sender, 204, Vec::new(), "")
        }
        _ => {
            send(
                sender,
                Message::Header(
                    405,
                    vec![("allow", ALLOW.into()), ("content-length", "0".into())],
                ),
            )?;
            send(sender, Message::End)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn propfind_expanded_names_and_output_retain_credit_until_their_owners_drop()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        const CHILD: &str = "RILL_DAV_PROPFIND_QUOTA_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new(std::env::current_exe()?).args(["webdav::methods::tests::propfind_expanded_names_and_output_retain_credit_until_their_owners_drop", "--exact", "--test-threads=1"]).env(CHILD,"1").status()?;
            assert!(status.success());
            return Ok(());
        }
        let names = (0..64).map(|n| format!("<p:n{n}/>")).collect::<String>();
        let body = format!(
            "<d:propfind xmlns:d=\"DAV:\" xmlns:p=\"{}\"><d:prop>{names}</d:prop></d:propfind>",
            "&amp;".repeat(160 * 1024)
        );
        let selection = selection(body.as_bytes())?;
        let mut output = PropfindBody::default();
        let failure = selection
            .names()
            .try_for_each(|name| output.name(name))
            .unwrap_err();
        assert_eq!(failure.status, 503);
        drop(output);
        assert!(super::super::budget::Memory::acquire(64 * 1024 * 1024).is_err());
        drop(selection);
        drop(super::super::budget::Memory::acquire(64 * 1024 * 1024)?);
        Ok(())
    }
}
