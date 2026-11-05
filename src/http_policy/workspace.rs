use regex_automata::nfa::thompson::{State, pikevm::PikeVM};
use std::sync::atomic::{AtomicUsize, Ordering};

const LIMIT: usize = 64 * 1024 * 1024;
static USED: AtomicUsize = AtomicUsize::new(0);

pub struct Permit(usize);
impl Drop for Permit {
    fn drop(&mut self) {
        USED.fetch_sub(self.0, Ordering::AcqRel);
    }
}

pub fn reserve(vm: &PikeVM) -> Option<Permit> {
    let nfa = vm.get_nfa();
    let states = nfa.states().len();
    let slots = nfa.group_info().slot_len();
    let edges = nfa.states().iter().try_fold(0_usize, |n, state| {
        n.checked_add(match state {
            State::Union { alternates } => alternates.len(),
            State::BinaryUnion { .. } => 2,
            _ => 0,
        })
    })?;
    // Two slot tables and sparse sets; doubled for Vec growth. The closure
    // stack visits each state once and holds at most one frame per edge or capture.
    let tables = states
        .checked_mul(slots)?
        .checked_add(slots.max(2))?
        .checked_mul(2)?
        .checked_mul(2)?
        .checked_mul(std::mem::size_of::<usize>())?;
    let sets = states
        .checked_mul(4)?
        .checked_mul(2)?
        .checked_mul(std::mem::size_of::<u32>())?;
    let stack = states
        .checked_add(edges)?
        .checked_add(1)?
        .checked_mul(2)?
        .checked_mul(32)?;
    let bytes = tables
        .checked_add(sets)?
        .checked_add(stack)?
        .checked_add(4096)?;
    USED.try_update(Ordering::AcqRel, Ordering::Acquire, |old| {
        old.checked_add(bytes).filter(|n| *n <= LIMIT)
    })
    .ok()?;
    Some(Permit(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_exhaustion_releases_capacity() {
        let vm = PikeVM::new("(hello|world)+").expect("valid regex");
        let mut permits = Vec::new();
        while let Some(permit) = reserve(&vm) {
            permits.push(permit);
        }
        assert!(!permits.is_empty());
        drop(permits.pop());
        assert!(reserve(&vm).is_some());
    }
}
