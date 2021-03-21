//! Kernel space probes.
use libc::pid_t;
use std::io;
use thiserror::Error;

use crate::{
    generated::bpf_prog_type::BPF_PROG_TYPE_KPROBE,
    programs::{
        load_program,
        probe::{attach, ProbeKind},
        LinkRef, ProgramData, ProgramError,
    },
};

/// A kernel probe.
///
/// Kernel probes are eBPF programs that can be attached to almost any function inside
/// the kernel. They can be of two kinds:
///
/// - `kprobe`: get attached to the *start* of the target functions
/// - `kretprobe`: get attached to the *return address* of the target functions
#[derive(Debug)]
pub struct KProbe {
    pub(crate) data: ProgramData,
    pub(crate) kind: ProbeKind,
}

impl KProbe {
    /// Loads the program inside the kernel.
    ///
    /// See also [`Program::load`](crate::programs::Program::load).
    pub fn load(&mut self) -> Result<(), ProgramError> {
        load_program(BPF_PROG_TYPE_KPROBE, &mut self.data)
    }

    /// Returns the name of the program.
    pub fn name(&self) -> String {
        self.data.name.to_string()
    }

    /// Returns `KProbe` if the program is a `kprobe`, or `KRetProbe` if the
    /// program is a `kretprobe`.
    pub fn kind(&self) -> ProbeKind {
        self.kind
    }

    /// Attaches the program.
    ///
    /// Attaches the probe to the given function name inside the kernel. If
    /// `offset` is non-zero, it is added to the address of the target
    /// function. If `pid` is `Some()`, the program executes only when the
    /// target function is triggered by the given `pid`.
    ///
    /// If the program is a `kprobe`, it is attached to the *start* address of the target function.
    /// Conversely if the program is a `kretprobe`, it is attached to the return address of the
    /// target function.
    ///
    ///
    /// # Example
    ///
    /// ```no_run
    /// # let mut bpf = Bpf::load_file("ebpf_programs.o")?;
    /// use aya::{Bpf, programs::KProbe};
    /// use std::convert::TryInto;
    ///
    /// let program: &mut KProbe = bpf.program_mut("intercept_wakeups")?.try_into()?;
    /// program.attach("try_to_wake_up", 0, None)?;
    /// # Ok::<(), aya::BpfError>(())
    /// ```
    ///
    pub fn attach(
        &mut self,
        fn_name: &str,
        offset: u64,
        pid: Option<pid_t>,
    ) -> Result<LinkRef, ProgramError> {
        attach(&mut self.data, self.kind, fn_name, offset, pid)
    }
}

#[derive(Debug, Error)]
pub enum KProbeError {
    #[error("`{filename}`")]
    FileError {
        filename: String,
        #[source]
        io_error: io::Error,
    },
}