use crate::{
    cannon::{
        Meta, Start, State, StepFrequency, VmConfiguration, PAGE_ADDRESS_MASK, PAGE_ADDRESS_SIZE,
        PAGE_SIZE,
    },
    keccak::{environment::KeccakEnv, E},
    mips::{
        column::Column,
        interpreter::{
            self, debugging::InstructionParts, ITypeInstruction, Instruction, InterpreterEnv,
            JTypeInstruction, RTypeInstruction,
        },
        registers::Registers,
    },
    preimage_oracle::PreImageOracle,
};
use ark_ff::Field;
use core::panic;
use kimchi::circuits::expr::ConstantExpr::Literal;
use log::{debug, info};
use std::array;

pub const NUM_GLOBAL_LOOKUP_TERMS: usize = 1;
pub const NUM_DECODING_LOOKUP_TERMS: usize = 2;
pub const NUM_INSTRUCTION_LOOKUP_TERMS: usize = 5;
pub const NUM_LOOKUP_TERMS: usize =
    NUM_GLOBAL_LOOKUP_TERMS + NUM_DECODING_LOOKUP_TERMS + NUM_INSTRUCTION_LOOKUP_TERMS;
pub const SCRATCH_SIZE: usize = 80; // TODO: Delete and use a vector instead

#[derive(Clone, Default)]
pub struct SyscallEnv {
    pub last_hint: Option<Vec<u8>>,
}

impl SyscallEnv {
    pub fn create(state: &State) -> Self {
        SyscallEnv {
            last_hint: state.last_hint.clone(),
        }
    }
}

pub struct Env<Fp> {
    pub instruction_counter: u32, // TODO: u32 will not be big enough..
    pub memory: Vec<(u32, Vec<u8>)>,
    pub last_memory_accesses: [usize; 3],
    pub memory_write_index: Vec<(u32, Vec<u32>)>, // TODO: u32 will not be big enough..
    pub last_memory_write_index_accesses: [usize; 3],
    pub registers: Registers<u32>,
    pub registers_write_index: Registers<u32>, // TODO: u32 will not be big enough..
    pub scratch_state_idx: usize,
    pub scratch_state: [Fp; SCRATCH_SIZE],
    pub halt: bool,
    pub syscall_env: SyscallEnv,
    pub preimage_oracle: PreImageOracle,
    pub keccak_env: Option<KeccakEnv<Fp>>,
}

fn fresh_scratch_state<Fp: Field, const N: usize>() -> [Fp; N] {
    array::from_fn(|_| Fp::zero())
}

const KUNIT: usize = 1024; // a kunit of memory is 1024 things (bytes, kilobytes, ...)
const PREFIXES: &str = "KMGTPE"; // prefixes for memory quantities KiB, MiB, GiB, ...

// Create a human-readable string representation of the memory size
fn memory_size(total: usize) -> String {
    if total < KUNIT {
        format!("{total} B")
    } else {
        // Compute the index in the prefixes string above
        let mut idx = 0;
        let mut d = KUNIT;
        let mut n = total / KUNIT;

        while n >= KUNIT {
            d *= KUNIT;
            idx += 1;
            n /= KUNIT;
        }

        let value = total as f64 / d as f64;

        let prefix =
        ////////////////////////////////////////////////////////////////////////////
        // Famous last words: 1023 exabytes ought to be enough for anybody        //
        //                                                                        //
        // Corollary: unwrap() below shouldn't fail                               //
        //                                                                        //
        // The maximum representation for usize corresponds to 16 exabytes anyway //
        ////////////////////////////////////////////////////////////////////////////
            PREFIXES.chars().nth(idx).unwrap();

        format!("{:.1} {}iB", value, prefix)
    }
}

impl<Fp: Field> InterpreterEnv for Env<Fp> {
    type Position = Column;

    fn alloc_scratch(&mut self) -> Self::Position {
        let scratch_idx = self.scratch_state_idx;
        self.scratch_state_idx += 1;
        Column::ScratchState(scratch_idx)
    }

    type Variable = u32;

    fn add_constraint(&mut self, _assert_equals_zero: Self::Variable) {
        // No-op for witness
        // Do not assert that _assert_equals_zero is zero here! Some variables may have
        // placeholders that do not faithfully represent the underlying values.
    }

    fn check_is_zero(assert_equals_zero: &Self::Variable) {
        assert_eq!(*assert_equals_zero, 0);
    }

    fn check_equal(x: &Self::Variable, y: &Self::Variable) {
        assert_eq!(*x, *y);
    }

    fn check_boolean(x: &Self::Variable) {
        if !(*x == 0 || *x == 1) {
            panic!("The value {} is not a boolean", *x);
        }
    }

    fn add_lookup(&mut self, _lookup: interpreter::Lookup<Self::Variable>) {
        // FIXME: Track the lookup values in the environment.
    }

    fn instruction_counter(&self) -> Self::Variable {
        self.instruction_counter
    }

    unsafe fn fetch_register(
        &mut self,
        idx: &Self::Variable,
        output: Self::Position,
    ) -> Self::Variable {
        let res = self.registers[*idx as usize];
        self.write_column(output, res.into());
        res
    }

    unsafe fn push_register_if(
        &mut self,
        idx: &Self::Variable,
        value: Self::Variable,
        if_is_true: &Self::Variable,
    ) {
        if *if_is_true == 1 {
            self.registers[*idx as usize] = value
        } else if *if_is_true == 0 {
            // No-op
        } else {
            panic!("Bad value for flag in push_register: {}", *if_is_true);
        }
    }

    unsafe fn fetch_register_access(
        &mut self,
        idx: &Self::Variable,
        output: Self::Position,
    ) -> Self::Variable {
        let res = self.registers_write_index[*idx as usize];
        self.write_column(output, res.into());
        res
    }

    unsafe fn push_register_access_if(
        &mut self,
        idx: &Self::Variable,
        value: Self::Variable,
        if_is_true: &Self::Variable,
    ) {
        if *if_is_true == 1 {
            self.registers_write_index[*idx as usize] = value
        } else if *if_is_true == 0 {
            // No-op
        } else {
            panic!("Bad value for flag in push_register: {}", *if_is_true);
        }
    }

    unsafe fn fetch_memory(
        &mut self,
        addr: &Self::Variable,
        output: Self::Position,
    ) -> Self::Variable {
        let page = addr >> PAGE_ADDRESS_SIZE;
        let page_address = (addr & PAGE_ADDRESS_MASK) as usize;
        let memory_page_idx = self.get_memory_page_index(page);
        let value = self.memory[memory_page_idx].1[page_address];
        self.write_column(output, value.into());
        value.into()
    }

    unsafe fn push_memory(&mut self, addr: &Self::Variable, value: Self::Variable) {
        let page = addr >> PAGE_ADDRESS_SIZE;
        let page_address = (addr & PAGE_ADDRESS_MASK) as usize;
        let memory_page_idx = self.get_memory_page_index(page);
        self.memory[memory_page_idx].1[page_address] =
            value.try_into().expect("push_memory values fit in a u8");
    }

    unsafe fn fetch_memory_access(
        &mut self,
        addr: &Self::Variable,
        output: Self::Position,
    ) -> Self::Variable {
        let page = addr >> PAGE_ADDRESS_SIZE;
        let page_address = (addr & PAGE_ADDRESS_MASK) as usize;
        let memory_write_index_page_idx = self.get_memory_access_page_index(page);
        let value = self.memory_write_index[memory_write_index_page_idx].1[page_address];
        self.write_column(output, value.into());
        value
    }

    unsafe fn push_memory_access(&mut self, addr: &Self::Variable, value: Self::Variable) {
        let page = addr >> PAGE_ADDRESS_SIZE;
        let page_address = (addr & PAGE_ADDRESS_MASK) as usize;
        let memory_write_index_page_idx = self.get_memory_access_page_index(page);
        self.memory_write_index[memory_write_index_page_idx].1[page_address] = value;
    }

    fn constant(x: u32) -> Self::Variable {
        x
    }

    unsafe fn bitmask(
        &mut self,
        x: &Self::Variable,
        highest_bit: u32,
        lowest_bit: u32,
        position: Self::Position,
    ) -> Self::Variable {
        let res = (x >> lowest_bit) & ((1 << (highest_bit - lowest_bit)) - 1);
        self.write_column(position, res.into());
        res
    }

    unsafe fn shift_left(
        &mut self,
        x: &Self::Variable,
        by: &Self::Variable,
        position: Self::Position,
    ) -> Self::Variable {
        let res = x << by;
        self.write_column(position, res.into());
        res
    }

    unsafe fn shift_right(
        &mut self,
        x: &Self::Variable,
        by: &Self::Variable,
        position: Self::Position,
    ) -> Self::Variable {
        let res = x >> by;
        self.write_column(position, res.into());
        res
    }

    unsafe fn shift_right_arithmetic(
        &mut self,
        x: &Self::Variable,
        by: &Self::Variable,
        position: Self::Position,
    ) -> Self::Variable {
        let res = ((*x as i32) >> by) as u32;
        self.write_column(position, res.into());
        res
    }

    unsafe fn test_zero(&mut self, x: &Self::Variable, position: Self::Position) -> Self::Variable {
        let res = if *x == 0 { 1 } else { 0 };
        self.write_column(position, res.into());
        res
    }

    unsafe fn inverse_or_zero(
        &mut self,
        x: &Self::Variable,
        position: Self::Position,
    ) -> Self::Variable {
        if *x == 0 {
            self.write_column(position, 0);
            0
        } else {
            self.write_field_column(position, Fp::from(*x as u64).inverse().unwrap());
            1 // Placeholder value
        }
    }

    unsafe fn test_less_than(
        &mut self,
        x: &Self::Variable,
        y: &Self::Variable,
        position: Self::Position,
    ) -> Self::Variable {
        let res = if *x < *y { 1 } else { 0 };
        self.write_column(position, res.into());
        res
    }

    unsafe fn test_less_than_signed(
        &mut self,
        x: &Self::Variable,
        y: &Self::Variable,
        position: Self::Position,
    ) -> Self::Variable {
        let res = if (*x as i32) < (*y as i32) { 1 } else { 0 };
        self.write_column(position, res.into());
        res
    }

    unsafe fn and_witness(
        &mut self,
        x: &Self::Variable,
        y: &Self::Variable,
        position: Self::Position,
    ) -> Self::Variable {
        let res = x & y;
        self.write_column(position, res.into());
        res
    }

    unsafe fn nor_witness(
        &mut self,
        x: &Self::Variable,
        y: &Self::Variable,
        position: Self::Position,
    ) -> Self::Variable {
        let res = !(x | y);
        self.write_column(position, res.into());
        res
    }

    unsafe fn or_witness(
        &mut self,
        x: &Self::Variable,
        y: &Self::Variable,
        position: Self::Position,
    ) -> Self::Variable {
        let res = x | y;
        self.write_column(position, res.into());
        res
    }

    unsafe fn xor_witness(
        &mut self,
        x: &Self::Variable,
        y: &Self::Variable,
        position: Self::Position,
    ) -> Self::Variable {
        let res = *x ^ *y;
        self.write_column(position, res.into());
        res
    }

    unsafe fn mul_signed_witness(
        &mut self,
        x: &Self::Variable,
        y: &Self::Variable,
        position: Self::Position,
    ) -> Self::Variable {
        let res = ((*x as i32) * (*y as i32)) as u32;
        self.write_column(position, res.into());
        res
    }

    unsafe fn mul_hi_lo_signed(
        &mut self,
        x: &Self::Variable,
        y: &Self::Variable,
        position_hi: Self::Position,
        position_lo: Self::Position,
    ) -> (Self::Variable, Self::Variable) {
        let mul = (((*x as i32) as i64) * ((*y as i32) as i64)) as u64;
        let hi = (mul >> 32) as u32;
        let lo = (mul & ((1 << 32) - 1)) as u32;
        self.write_column(position_hi, hi.into());
        self.write_column(position_lo, lo.into());
        (hi, lo)
    }

    unsafe fn mul_hi_lo(
        &mut self,
        x: &Self::Variable,
        y: &Self::Variable,
        position_hi: Self::Position,
        position_lo: Self::Position,
    ) -> (Self::Variable, Self::Variable) {
        let mul = (*x as u64) * (*y as u64);
        let hi = (mul >> 32) as u32;
        let lo = (mul & ((1 << 32) - 1)) as u32;
        self.write_column(position_hi, hi.into());
        self.write_column(position_lo, lo.into());
        (hi, lo)
    }

    unsafe fn divmod_signed(
        &mut self,
        x: &Self::Variable,
        y: &Self::Variable,
        position_quotient: Self::Position,
        position_remainder: Self::Position,
    ) -> (Self::Variable, Self::Variable) {
        let q = ((*x as i32) / (*y as i32)) as u32;
        let r = ((*x as i32) % (*y as i32)) as u32;
        self.write_column(position_quotient, q.into());
        self.write_column(position_remainder, r.into());
        (q, r)
    }

    unsafe fn divmod(
        &mut self,
        x: &Self::Variable,
        y: &Self::Variable,
        position_quotient: Self::Position,
        position_remainder: Self::Position,
    ) -> (Self::Variable, Self::Variable) {
        let q = x / y;
        let r = x % y;
        self.write_column(position_quotient, q.into());
        self.write_column(position_remainder, r.into());
        (q, r)
    }

    unsafe fn count_leading_zeros(
        &mut self,
        x: &Self::Variable,
        position: Self::Position,
    ) -> Self::Variable {
        let res = x.leading_zeros();
        self.write_column(position, res.into());
        res
    }

    fn copy(&mut self, x: &Self::Variable, position: Self::Position) -> Self::Variable {
        self.write_column(position, (*x).into());
        *x
    }

    fn set_halted(&mut self, flag: Self::Variable) {
        if flag == 0 {
            self.halt = false
        } else if flag == 1 {
            self.halt = true
        } else {
            panic!("Bad value for flag in set_halted: {}", flag);
        }
    }

    fn report_exit(&mut self, exit_code: &Self::Variable) {
        println!("Exited with code {}", *exit_code);
    }
}

impl<Fp: Field> Env<Fp> {
    pub fn create(page_size: usize, state: State, preimage_oracle: PreImageOracle) -> Self {
        let initial_instruction_pointer = state.pc;
        let next_instruction_pointer = state.next_pc;

        let syscall_env = SyscallEnv::create(&state);

        let mut initial_memory: Vec<(u32, Vec<u8>)> = state
            .memory
            .into_iter()
            // Check that the conversion from page data is correct
            .map(|page| (page.index, page.data))
            .collect();

        for (_address, initial_memory) in initial_memory.iter_mut() {
            initial_memory.extend((0..(page_size - initial_memory.len())).map(|_| 0u8));
            assert_eq!(initial_memory.len(), page_size);
        }

        let memory_offsets = initial_memory
            .iter()
            .map(|(offset, _)| *offset)
            .collect::<Vec<_>>();

        let initial_registers = {
            let preimage_key = {
                let mut preimage_key = [0u32; 8];
                for (i, preimage_key_word) in preimage_key.iter_mut().enumerate() {
                    *preimage_key_word = u32::from_be_bytes(
                        state.preimage_key[i * 4..(i + 1) * 4].try_into().unwrap(),
                    )
                }
                preimage_key
            };
            Registers {
                lo: state.lo,
                hi: state.hi,
                general_purpose: state.registers,
                current_instruction_pointer: initial_instruction_pointer,
                next_instruction_pointer,
                heap_pointer: state.heap,
                preimage_key,
                preimage_offset: state.preimage_offset,
            }
        };

        Env {
            instruction_counter: state.step as u32,
            memory: initial_memory.clone(),
            last_memory_accesses: [0usize; 3],
            memory_write_index: memory_offsets
                .iter()
                .map(|offset| (*offset, vec![0u32; page_size]))
                .collect(),
            last_memory_write_index_accesses: [0usize; 3],
            registers: initial_registers.clone(),
            registers_write_index: Registers::default(),
            scratch_state_idx: 0,
            scratch_state: fresh_scratch_state(),
            halt: state.exited,
            syscall_env,
            preimage_oracle,
            keccak_env: None,
        }
    }

    pub fn reset_scratch_state(&mut self) {
        self.scratch_state_idx = 0;
        self.scratch_state = fresh_scratch_state();
    }

    pub fn write_column(&mut self, column: Column, value: u64) {
        self.write_field_column(column, value.into())
    }

    pub fn write_field_column(&mut self, column: Column, value: Fp) {
        match column {
            Column::ScratchState(idx) => self.scratch_state[idx] = value,
            Column::KeccakState(col) => {
                if let Some(keccak_env) = &mut self.keccak_env {
                    keccak_env.keccak_state[col] = E::constant(Literal(value))
                } else {
                    panic!("Keccak state not initialized")
                }
            }
        }
    }

    pub fn update_last_memory_access(&mut self, i: usize) {
        let [i_0, i_1, _] = self.last_memory_accesses;
        self.last_memory_accesses = [i, i_0, i_1]
    }

    pub fn get_memory_page_index(&mut self, page: u32) -> usize {
        for &i in self.last_memory_accesses.iter() {
            if self.memory_write_index[i].0 == page {
                return i;
            }
        }
        for (i, (page_index, _memory)) in self.memory.iter_mut().enumerate() {
            if *page_index == page {
                self.update_last_memory_access(i);
                return i;
            }
        }

        // Memory not found; dynamically allocate
        let memory = vec![0u8; PAGE_SIZE as usize];
        self.memory.push((page, memory));
        let i = self.memory.len() - 1;
        self.update_last_memory_access(i);
        i
    }

    pub fn update_last_memory_write_index_access(&mut self, i: usize) {
        let [i_0, i_1, _] = self.last_memory_write_index_accesses;
        self.last_memory_write_index_accesses = [i, i_0, i_1]
    }

    pub fn get_memory_access_page_index(&mut self, page: u32) -> usize {
        for &i in self.last_memory_write_index_accesses.iter() {
            if self.memory_write_index[i].0 == page {
                return i;
            }
        }
        for (i, (page_index, _memory_write_index)) in self.memory_write_index.iter_mut().enumerate()
        {
            if *page_index == page {
                self.update_last_memory_write_index_access(i);
                return i;
            }
        }

        // Memory not found; dynamically allocate
        let memory_write_index = vec![0u32; PAGE_SIZE as usize];
        self.memory_write_index.push((page, memory_write_index));
        let i = self.memory_write_index.len() - 1;
        self.update_last_memory_write_index_access(i);
        i
    }

    pub fn get_memory_direct(&mut self, addr: u32) -> u8 {
        let page = addr >> PAGE_ADDRESS_SIZE;
        let page_address = (addr & PAGE_ADDRESS_MASK) as usize;
        let memory_idx = self.get_memory_page_index(page);
        self.memory[memory_idx].1[page_address]
    }

    pub fn decode_instruction(&mut self) -> (Instruction, u32) {
        let instruction =
            ((self.get_memory_direct(self.registers.current_instruction_pointer) as u32) << 24)
                | ((self.get_memory_direct(self.registers.current_instruction_pointer + 1) as u32)
                    << 16)
                | ((self.get_memory_direct(self.registers.current_instruction_pointer + 2) as u32)
                    << 8)
                | (self.get_memory_direct(self.registers.current_instruction_pointer + 3) as u32);
        let opcode = {
            match instruction >> 26 {
                0x00 => match instruction & 0x3F {
                    0x00 => Instruction::RType(RTypeInstruction::ShiftLeftLogical),
                    0x02 => Instruction::RType(RTypeInstruction::ShiftRightLogical),
                    0x03 => Instruction::RType(RTypeInstruction::ShiftRightArithmetic),
                    0x04 => Instruction::RType(RTypeInstruction::ShiftLeftLogicalVariable),
                    0x06 => Instruction::RType(RTypeInstruction::ShiftRightLogicalVariable),
                    0x07 => Instruction::RType(RTypeInstruction::ShiftRightArithmeticVariable),
                    0x08 => Instruction::RType(RTypeInstruction::JumpRegister),
                    0x09 => Instruction::RType(RTypeInstruction::JumpAndLinkRegister),
                    0x0a => Instruction::RType(RTypeInstruction::MoveZero),
                    0x0b => Instruction::RType(RTypeInstruction::MoveNonZero),
                    0x0c => match self.registers.general_purpose[2] {
                        4090 => Instruction::RType(RTypeInstruction::SyscallMmap),
                        4045 => {
                            // sysBrk
                            Instruction::RType(RTypeInstruction::SyscallOther)
                        }
                        4120 => {
                            // sysClone
                            Instruction::RType(RTypeInstruction::SyscallOther)
                        }
                        4246 => Instruction::RType(RTypeInstruction::SyscallExitGroup),
                        4003 => match self.registers.general_purpose[4] {
                            interpreter::FD_HINT_READ => {
                                Instruction::RType(RTypeInstruction::SyscallReadHint)
                            }
                            interpreter::FD_PREIMAGE_READ => {
                                Instruction::RType(RTypeInstruction::SyscallReadPreimage)
                            }
                            _ => Instruction::RType(RTypeInstruction::SyscallReadOther),
                        },
                        4004 => match self.registers.general_purpose[4] {
                            interpreter::FD_PREIMAGE_WRITE => {
                                Instruction::RType(RTypeInstruction::SyscallWritePreimage)
                            }
                            interpreter::FD_HINT_WRITE => {
                                Instruction::RType(RTypeInstruction::SyscallWriteHint)
                            }
                            _ => Instruction::RType(RTypeInstruction::SyscallWriteOther),
                        },
                        4055 => Instruction::RType(RTypeInstruction::SyscallFcntl),
                        _ => {
                            // NB: This has well-defined behavior. Don't panic!
                            Instruction::RType(RTypeInstruction::SyscallOther)
                        }
                    },
                    0x0f => Instruction::RType(RTypeInstruction::Sync),
                    0x10 => Instruction::RType(RTypeInstruction::MoveFromHi),
                    0x11 => Instruction::RType(RTypeInstruction::MoveToHi),
                    0x12 => Instruction::RType(RTypeInstruction::MoveFromLo),
                    0x13 => Instruction::RType(RTypeInstruction::MoveToLo),
                    0x18 => Instruction::RType(RTypeInstruction::Multiply),
                    0x19 => Instruction::RType(RTypeInstruction::MultiplyUnsigned),
                    0x1a => Instruction::RType(RTypeInstruction::Div),
                    0x1b => Instruction::RType(RTypeInstruction::DivUnsigned),
                    0x20 => Instruction::RType(RTypeInstruction::Add),
                    0x21 => Instruction::RType(RTypeInstruction::AddUnsigned),
                    0x22 => Instruction::RType(RTypeInstruction::Sub),
                    0x23 => Instruction::RType(RTypeInstruction::SubUnsigned),
                    0x24 => Instruction::RType(RTypeInstruction::And),
                    0x25 => Instruction::RType(RTypeInstruction::Or),
                    0x26 => Instruction::RType(RTypeInstruction::Xor),
                    0x27 => Instruction::RType(RTypeInstruction::Nor),
                    0x2a => Instruction::RType(RTypeInstruction::SetLessThan),
                    0x2b => Instruction::RType(RTypeInstruction::SetLessThanUnsigned),
                    _ => {
                        panic!("Unhandled instruction {:#X}", instruction)
                    }
                },
                0x01 => {
                    // RegImm instructions
                    match (instruction >> 16) & 0x1F {
                        0x0 => Instruction::IType(ITypeInstruction::BranchLtZero),
                        0x1 => Instruction::IType(ITypeInstruction::BranchGeqZero),
                        _ => panic!("Unhandled instruction {:#X}", instruction),
                    }
                }
                0x02 => Instruction::JType(JTypeInstruction::Jump),
                0x03 => Instruction::JType(JTypeInstruction::JumpAndLink),
                0x04 => Instruction::IType(ITypeInstruction::BranchEq),
                0x05 => Instruction::IType(ITypeInstruction::BranchNeq),
                0x06 => Instruction::IType(ITypeInstruction::BranchLeqZero),
                0x07 => Instruction::IType(ITypeInstruction::BranchGtZero),
                0x08 => Instruction::IType(ITypeInstruction::AddImmediate),
                0x09 => Instruction::IType(ITypeInstruction::AddImmediateUnsigned),
                0x0A => Instruction::IType(ITypeInstruction::SetLessThanImmediate),
                0x0B => Instruction::IType(ITypeInstruction::SetLessThanImmediateUnsigned),
                0x0C => Instruction::IType(ITypeInstruction::AndImmediate),
                0x0D => Instruction::IType(ITypeInstruction::OrImmediate),
                0x0E => Instruction::IType(ITypeInstruction::XorImmediate),
                0x0F => Instruction::IType(ITypeInstruction::LoadUpperImmediate),
                0x1C => match instruction & 0x3F {
                    0x02 => Instruction::RType(RTypeInstruction::MultiplyToRegister),
                    0x20 => Instruction::RType(RTypeInstruction::CountLeadingZeros),
                    0x21 => Instruction::RType(RTypeInstruction::CountLeadingOnes),
                    _ => panic!("Unhandled instruction {:#X}", instruction),
                },
                0x20 => Instruction::IType(ITypeInstruction::Load8),
                0x21 => Instruction::IType(ITypeInstruction::Load16),
                0x22 => Instruction::IType(ITypeInstruction::LoadWordLeft),
                0x23 => Instruction::IType(ITypeInstruction::Load32),
                0x24 => Instruction::IType(ITypeInstruction::Load8Unsigned),
                0x25 => Instruction::IType(ITypeInstruction::Load16Unsigned),
                0x26 => Instruction::IType(ITypeInstruction::LoadWordRight),
                0x28 => Instruction::IType(ITypeInstruction::Store8),
                0x29 => Instruction::IType(ITypeInstruction::Store16),
                0x2a => Instruction::IType(ITypeInstruction::StoreWordLeft),
                0x2b => Instruction::IType(ITypeInstruction::Store32),
                0x2e => Instruction::IType(ITypeInstruction::StoreWordRight),
                0x30 => {
                    // Note: This is ll (LoadLinked), but we're only simulating a single processor.
                    Instruction::IType(ITypeInstruction::Load32)
                }
                0x38 => {
                    // Note: This is sc (StoreConditional), but we're only simulating a single processor.
                    Instruction::IType(ITypeInstruction::Store32Conditional)
                }
                _ => {
                    panic!("Unhandled instruction {:#X}", instruction)
                }
            }
        };
        (opcode, instruction)
    }

    pub fn step(&mut self, config: &VmConfiguration, metadata: &Meta, start: &Start) {
        self.reset_scratch_state();
        let (opcode, instruction) = self.decode_instruction();
        let instruction_parts: InstructionParts = InstructionParts::decode(instruction);
        debug!("instruction: {:?}", opcode);
        debug!("Instruction hex: {:#010x}", instruction);
        debug!("Instruction: {:#034b}", instruction);
        debug!("Rs: {:#07b}", instruction_parts.rs);
        debug!("Rt: {:#07b}", instruction_parts.rt);
        debug!("Rd: {:#07b}", instruction_parts.rd);
        debug!("Shamt: {:#07b}", instruction_parts.shamt);
        debug!("Funct: {:#08b}", instruction_parts.funct);

        self.pp_info(&config.info_at, metadata, start);

        // Force stops at given iteration
        if self.should_trigger_at(&config.stop_at) {
            self.halt = true;
            return;
        }

        interpreter::interpret_instruction(self, opcode);

        self.instruction_counter += 1;
    }

    fn should_trigger_at(&self, at: &StepFrequency) -> bool {
        let m: u64 = self.instruction_counter as u64;
        match at {
            StepFrequency::Never => false,
            StepFrequency::Always => true,
            StepFrequency::Exactly(n) => *n == m,
            StepFrequency::Every(n) => m % *n == 0,
        }
    }

    // Compute memory usage
    fn memory_usage(&self) -> String {
        let total = self.memory.len() * PAGE_SIZE as usize;
        memory_size(total)
    }

    fn page_address(&self) -> (u32, usize) {
        let address = self.registers.current_instruction_pointer;
        let page = address >> PAGE_ADDRESS_SIZE;
        let page_address = (address & PAGE_ADDRESS_MASK) as usize;
        (page, page_address)
    }

    fn get_opcode(&mut self) -> Option<u32> {
        let (page_id, page_address) = self.page_address();
        for (page_index, memory) in self.memory.iter() {
            if page_id == *page_index {
                let memory_slice: [u8; 4] = memory[page_address..page_address + 4]
                    .try_into()
                    .expect("Couldn't read 4 bytes at given address");
                return Some(u32::from_be_bytes(memory_slice));
            }
        }
        None
    }

    fn pp_info(&mut self, at: &StepFrequency, meta: &Meta, start: &Start) {
        if self.should_trigger_at(at) {
            let elapsed = start.time.elapsed();
            let step = self.instruction_counter;
            let pc = self.registers.current_instruction_pointer;

            // Get the 32-bits opcode
            let insn = self.get_opcode().unwrap();

            // Approximate instruction per seconds
            let how_many_steps = step as usize - start.step;
            let ips = how_many_steps as f64 / elapsed.as_secs() as f64;

            let pages = self.memory.len();

            let mem = self.memory_usage();
            let name = meta
                .find_address_symbol(pc)
                .unwrap_or_else(|| "n/a".to_string());

            info!(
                "processing step={} pc={:010x} insn={:010x} ips={:.2} pages={} mem={} name={}",
                step, pc, insn, ips, pages, mem, name
            );
        }
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn test_memory_size() {
        assert_eq!(memory_size(1023_usize), "1023 B");
        assert_eq!(memory_size(1024_usize), "1.0 KiB");
        assert_eq!(memory_size(1024 * 1024_usize), "1.0 MiB");
        assert_eq!(memory_size(2100 * 1024 * 1024_usize), "2.1 GiB");
        assert_eq!(memory_size(std::usize::MAX), "16.0 EiB");
    }
}
