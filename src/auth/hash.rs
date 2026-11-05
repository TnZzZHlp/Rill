use argon2::password_hash::PasswordHash;
use base64::Engine;

pub fn valid(value: &str) -> bool {
    if let Some(rest) = value.strip_prefix("$2b$") {
        if rest.len() != 56 || rest.as_bytes().get(2) != Some(&b'$') {
            return false;
        }
        let Some(cost) = rest.get(..2).and_then(|s| s.parse::<u32>().ok()) else {
            return false;
        };
        if !(10..=14).contains(&cost) {
            return false;
        }
        let Some(salt) = rest.get(3..25) else {
            return false;
        };
        let Some(hash) = rest.get(25..) else {
            return false;
        };
        return bcrypt::BASE_64.decode(salt).is_ok_and(|s| s.len() == 16)
            && bcrypt::BASE_64.decode(hash).is_ok_and(|s| s.len() == 23);
    }
    let Ok(hash) = PasswordHash::new(value) else {
        return false;
    };
    if hash.algorithm.as_str() != "argon2id" || hash.version != Some(19) {
        return false;
    }
    let mut memory = None;
    let mut time = None;
    let mut parallel = None;
    for (name, value) in hash.params.iter() {
        let Ok(n) = value.decimal() else {
            return false;
        };
        match name.as_str() {
            "m" if memory.is_none() => memory = Some(n),
            "t" if time.is_none() => time = Some(n),
            "p" if parallel.is_none() => parallel = Some(n),
            _ => return false,
        }
    }
    if !memory.is_some_and(|n| (8192..=65536).contains(&n))
        || !time.is_some_and(|n| (1..=3).contains(&n))
        || !parallel.is_some_and(|n| (1..=2).contains(&n))
    {
        return false;
    }
    let mut salt = [0u8; 64];
    hash.salt.is_some_and(|s| {
        s.decode_b64(&mut salt)
            .is_ok_and(|b| (8..=64).contains(&b.len()))
    }) && hash.hash.is_some_and(|h| (4..=64).contains(&h.len()))
        && argon2::Params::try_from(&hash).is_ok()
}
