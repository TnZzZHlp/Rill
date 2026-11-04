#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Identity([u8; 16]);
impl Identity {
    pub fn from_config(canonical_site: &str, route_pattern: &str) -> Self {
        let mut hash = openssl::sha::Sha256::new();
        hash.update(&(canonical_site.len() as u64).to_be_bytes());
        hash.update(canonical_site.as_bytes());
        hash.update(&(route_pattern.len() as u64).to_be_bytes());
        hash.update(route_pattern.as_bytes());
        let digest = hash.finish();
        let mut id = [0; 16];
        id.copy_from_slice(&digest[..16]);
        Self(id)
    }
    pub fn hex(self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut s = String::with_capacity(32);
        for b in self.0 {
            s.push(HEX[(b >> 4) as usize] as char);
            s.push(HEX[(b & 15) as usize] as char);
        }
        s
    }
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Method {
    Get,
    Head,
    Post,
    Put,
    Delete,
    Options,
    Patch,
    Connect,
    Other,
}
