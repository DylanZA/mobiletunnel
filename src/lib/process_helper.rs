use libc::{prctl, PR_SET_PDEATHSIG, SIGKILL};

pub fn set_kill_on_parent_death() {
    unsafe {
        prctl(PR_SET_PDEATHSIG, SIGKILL);
    }
}
