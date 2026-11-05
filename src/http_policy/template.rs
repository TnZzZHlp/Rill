use std::net::IpAddr;

#[derive(Clone, Debug)]
pub struct Template {
    parts: Vec<Part>,
}
#[derive(Clone, Debug)]
enum Part {
    Literal(String),
    ClientIp,
    UpstreamAuthority,
    Path,
    Site,
    Capture(usize),
}
#[derive(Debug)]
pub enum InvalidTemplate {
    Invalid,
    UnavailableTemplate,
}

pub struct Context<'a> {
    pub client_ip: IpAddr,
    pub upstream_authority: Option<&'a str>,
    pub path: &'a str,
    pub site: &'a str,
    pub captures: [Option<&'a str>; 9],
}
impl Template {
    pub fn compile(mut input: &str) -> Result<Self, InvalidTemplate> {
        let mut parts = Vec::new();
        while !input.is_empty() {
            let pos = input.find(['{', '}']).unwrap_or(input.len());
            if pos > 0 {
                let text = &input[..pos];
                http::HeaderValue::from_str(text).map_err(|_| InvalidTemplate::Invalid)?;
                parts.push(Part::Literal(text.to_owned()));
                input = &input[pos..];
            }
            if input.is_empty() {
                break;
            }
            if !input.starts_with('{') {
                return Err(InvalidTemplate::Invalid);
            }
            let end = input.find('}').ok_or(InvalidTemplate::Invalid)?;
            let name = &input[1..end];
            let part = match name {
                "client-ip" => Part::ClientIp,
                "upstream-authority" => Part::UpstreamAuthority,
                "path" => Part::Path,
                "site" => Part::Site,
                _ if name.len() == 9
                    && name.starts_with("capture.")
                    && (b'1'..=b'9').contains(&name.as_bytes()[8]) =>
                {
                    Part::Capture(usize::from(name.as_bytes()[8] - b'1'))
                }
                _ => return Err(InvalidTemplate::Invalid),
            };
            parts.push(part);
            input = &input[end + 1..];
        }
        Ok(Self { parts })
    }
    pub fn evaluate(&self, ctx: &Context<'_>, limit: usize) -> Result<String, InvalidTemplate> {
        let mut result = String::new();
        let ip = ctx.client_ip.to_string();
        for part in &self.parts {
            let value = match part {
                Part::Literal(s) => s.as_str(),
                Part::ClientIp => &ip,
                Part::UpstreamAuthority => ctx
                    .upstream_authority
                    .ok_or(InvalidTemplate::UnavailableTemplate)?,
                Part::Path => ctx.path,
                Part::Site => ctx.site,
                Part::Capture(i) => ctx.captures[*i].unwrap_or(""),
            };
            if result
                .len()
                .checked_add(value.len())
                .is_none_or(|n| n > limit)
            {
                return Err(InvalidTemplate::Invalid);
            }
            result.push_str(value);
        }
        http::HeaderValue::from_str(&result).map_err(|_| InvalidTemplate::Invalid)?;
        Ok(result)
    }
    pub fn allocated_bytes(&self) -> usize {
        self.parts
            .capacity()
            .saturating_mul(std::mem::size_of::<Part>())
            .saturating_add(
                self.parts
                    .iter()
                    .map(|p| match p {
                        Part::Literal(s) => s.capacity(),
                        _ => 0,
                    })
                    .sum::<usize>(),
            )
    }
}
