use super::{
    compile::PreparedPolicy,
    execute::{ActionFlow, MatchContext, Outcome, PathErrorClass},
    model::Action,
};

pub(super) fn execute(
    a: &Action,
    ctx: &mut MatchContext<'_>,
    policy: &PreparedPolicy,
) -> Result<Option<ActionFlow>, ()> {
    match a {
        Action::Rewrite(template) => {
            if super::path::validate(&ctx.path).is_err() {
                return Ok(Some(ActionFlow::Terminal(Outcome::PathFailure {
                    class: PathErrorClass::InvalidInput,
                    code: "POLICY_INVALID_PATH",
                    status: 400,
                })));
            }
            let state = ctx.state.as_deref_mut().ok_or(())?;
            match state.rewrite(template) {
                Ok(path) => {
                    ctx.path = path;
                    Ok(None)
                }
                Err(()) => Ok(Some(ActionFlow::Terminal(Outcome::PathFailure {
                    class: PathErrorClass::InvalidInput,
                    code: "POLICY_INVALID_PATH",
                    status: 400,
                }))),
            }
        }
        Action::StripPrefix(prefix) => match super::path::strip(&ctx.path, prefix) {
            Ok(Some(path)) => {
                ctx.state.as_deref_mut().ok_or(())?.set_path(path.clone());
                ctx.path = path;
                Ok(None)
            }
            Ok(None) => Ok(Some(ActionFlow::Terminal(Outcome::PathFailure {
                class: PathErrorClass::LocalNotFound,
                code: "POLICY_PATH_MISS",
                status: 404,
            }))),
            Err(_) => Ok(Some(ActionFlow::Terminal(Outcome::PathFailure {
                class: PathErrorClass::InvalidInput,
                code: "POLICY_INVALID_PATH",
                status: 400,
            }))),
        },
        Action::TryFiles { root, candidates } => {
            if super::path::validate(&ctx.path).is_err() {
                return Ok(Some(ActionFlow::Terminal(Outcome::PathFailure {
                    class: PathErrorClass::InvalidInput,
                    code: "POLICY_INVALID_PATH",
                    status: 400,
                })));
            }
            let root = policy
                .roots
                .get(&(root.origin.source.path.clone(), root.origin.offset))
                .ok_or(())?;
            let state = ctx.state.as_deref_mut().ok_or(())?;
            for candidate in candidates {
                let path = match state.expand_path(candidate, 4096) {
                    Ok(path) => path,
                    Err(()) => {
                        return Ok(Some(ActionFlow::Terminal(Outcome::PathFailure {
                            class: PathErrorClass::InvalidInput,
                            code: "POLICY_INVALID_PATH",
                            status: 400,
                        })));
                    }
                };
                let relative = crate::static_files::path::decode(&path).map_err(|_| ())?;
                match root.open(&relative) {
                    Ok(file) => match file.metadata() {
                        Ok(metadata) if metadata.is_file() => {
                            state.set_path(path.clone());
                            ctx.path = path;
                            return Ok(None);
                        }
                        Ok(_) => {
                            return Ok(Some(ActionFlow::Terminal(Outcome::PathFailure {
                                class: PathErrorClass::InvalidInput,
                                code: "POLICY_FILE_IO",
                                status: 403,
                            })));
                        }
                        Err(_) => {
                            return Ok(Some(ActionFlow::Terminal(Outcome::PathFailure {
                                class: PathErrorClass::LocalUnavailable,
                                code: "POLICY_FILE_IO",
                                status: 503,
                            })));
                        }
                    },
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                    Err(error) => {
                        let (class, status) =
                            match error.raw_os_error().map(nix::errno::Errno::from_raw) {
                                Some(
                                    nix::errno::Errno::EACCES
                                    | nix::errno::Errno::EPERM
                                    | nix::errno::Errno::EXDEV
                                    | nix::errno::Errno::ELOOP
                                    | nix::errno::Errno::EAGAIN
                                    | nix::errno::Errno::ENOTDIR,
                                ) => (PathErrorClass::InvalidInput, 403),
                                Some(
                                    nix::errno::Errno::EINVAL | nix::errno::Errno::ENAMETOOLONG,
                                ) => (PathErrorClass::InvalidInput, 400),
                                Some(
                                    nix::errno::Errno::EMFILE
                                    | nix::errno::Errno::ENFILE
                                    | nix::errno::Errno::ENOMEM,
                                ) => (PathErrorClass::Capacity, 503),
                                _ => (PathErrorClass::LocalUnavailable, 503),
                            };
                        return Ok(Some(ActionFlow::Terminal(Outcome::PathFailure {
                            class,
                            code: "POLICY_FILE_IO",
                            status,
                        })));
                    }
                }
            }
            Ok(Some(ActionFlow::Terminal(Outcome::PathFailure {
                class: PathErrorClass::LocalNotFound,
                code: "POLICY_FILE_MISS",
                status: 404,
            })))
        }
        _ => Err(()),
    }
}
