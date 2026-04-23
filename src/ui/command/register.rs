use crate::debugger::register::RegisterMap;
use crate::debugger::{Debugger, register};
use crate::ui::command;
use crate::ui::command::CommandError;
use register::Register as Reg;

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Info,
    Read(String),
    Write(String, u64),
}

pub struct Handler<'a> {
    dbg: &'a Debugger,
}

pub struct RegisterValue {
    pub register_name: String,
    pub value: u64,
}

pub type ExecutionResult = Vec<RegisterValue>;

impl<'a> Handler<'a> {
    pub fn new(debugger: &'a Debugger) -> Self {
        Self { dbg: debugger }
    }

    pub fn handle(self, cmd: &Command) -> command::CommandResult<ExecutionResult> {
        match cmd {
            Command::Info => {
                #[cfg(target_arch = "x86_64")]
                let registers_to_dump: &[Reg] = &[
                    Reg::Rax,
                    Reg::Rbx,
                    Reg::Rcx,
                    Reg::Rdx,
                    Reg::Rdi,
                    Reg::Rsi,
                    Reg::Rbp,
                    Reg::Rsp,
                    Reg::R8,
                    Reg::R9,
                    Reg::R10,
                    Reg::R11,
                    Reg::R12,
                    Reg::R13,
                    Reg::R14,
                    Reg::R15,
                    Reg::Rip,
                    Reg::Eflags,
                    Reg::Cs,
                    Reg::OrigRax,
                    Reg::FsBase,
                    Reg::GsBase,
                    Reg::Fs,
                    Reg::Gs,
                    Reg::Ss,
                    Reg::Ds,
                    Reg::Es,
                ];

                #[cfg(target_arch = "aarch64")]
                let registers_to_dump: &[Reg] = &[
                    Reg::X0,
                    Reg::X1,
                    Reg::X2,
                    Reg::X3,
                    Reg::X4,
                    Reg::X5,
                    Reg::X6,
                    Reg::X7,
                    Reg::X8,
                    Reg::X9,
                    Reg::X10,
                    Reg::X11,
                    Reg::X12,
                    Reg::X13,
                    Reg::X14,
                    Reg::X15,
                    Reg::X16,
                    Reg::X17,
                    Reg::X18,
                    Reg::X19,
                    Reg::X20,
                    Reg::X21,
                    Reg::X22,
                    Reg::X23,
                    Reg::X24,
                    Reg::X25,
                    Reg::X26,
                    Reg::X27,
                    Reg::X28,
                    Reg::X29,
                    Reg::X30,
                    Reg::Sp,
                    Reg::Pc,
                    Reg::Pstate,
                ];

                let register_map = RegisterMap::current(self.dbg.ecx().pid_on_focus())
                    .map_err(CommandError::Handle)?;

                Ok(registers_to_dump
                    .iter()
                    .map(|&r| RegisterValue {
                        register_name: r.to_string(),
                        value: register_map.value(r),
                    })
                    .collect::<Vec<_>>())
            }
            Command::Read(register) => Ok(vec![RegisterValue {
                register_name: register.to_string(),
                value: self.dbg.get_register_value(register)?,
            }]),
            Command::Write(register, value) => {
                self.dbg.set_register_value(register, *value)?;
                Ok(vec![])
            }
        }
    }
}
