//! TEST-ONLY reference for the polyphase synthesis: the instruction-level transcription of Apple's `_IDCT32PLONKAS`
//! (`synth_machine.rs`, generated from the AArch64 disassembly by `tools/gen_plonkas.py`: register file, NZCV flags, one
//! statement per instruction) run on a byte-addressed memory model that mirrors Apple's (window table at its original
//! virtual address, private argument stack, staging area, two 64-byte aligned ring buffers whose phase lives in the low
//! bits of the ring pointer).  It is compiled only with `cfg(test)`; `synth.rs` is the structured implementation that is
//! proven equivalent to it (tests in `synth.rs` and the large randomized comparison in `Mp3XBack/synthcmp`).

use super::super::back_tables::{SYNTH_TABLE, SYNTH_TABLE_VA};

#[path = "synth_machine.rs"]
mod machine;

const ARENA_VA: u64 = 0x1000_0000;
const STACK_TOP: u64 = ARENA_VA + 0x400;
const IN_OFF: usize = 0x1000;
const IN_BYTES: usize = 0x1200;
const V_OFF: usize = 0x4000;
const V_CH_BYTES: usize = 0x880;
const ARENA_BYTES: usize = V_OFF + 2 * V_CH_BYTES + 0x200;
const DCT_CONST_VA: u64 = 0x22f9c;

/// Byte-addressed memory seen by the machine-level transcription.
pub(crate) struct Mem {
    arena: Vec<u8>,
}

impl Mem {
    fn new() -> Mem {
        Mem { arena: vec![0; ARENA_BYTES] }
    }

    #[inline(always)]
    fn arena_idx(&self, a: u64) -> usize {
        a.wrapping_sub(ARENA_VA) as usize
    }

    pub(crate) fn r32(&self, a: u64) -> u32 {
        if a >= ARENA_VA {
            let o = self.arena_idx(a);
            u32::from_le_bytes(self.arena[o..o + 4].try_into().unwrap())
        } else {
            let o = a.wrapping_sub(SYNTH_TABLE_VA) as usize;
            u32::from_le_bytes(SYNTH_TABLE[o..o + 4].try_into().unwrap())
        }
    }

    pub(crate) fn r64(&self, a: u64) -> u64 {
        if a >= ARENA_VA {
            let o = self.arena_idx(a);
            u64::from_le_bytes(self.arena[o..o + 8].try_into().unwrap())
        } else {
            let o = a.wrapping_sub(SYNTH_TABLE_VA) as usize;
            u64::from_le_bytes(SYNTH_TABLE[o..o + 8].try_into().unwrap())
        }
    }

    pub(crate) fn w16(&mut self, a: u64, v: u16) {
        let o = self.arena_idx(a);
        self.arena[o..o + 2].copy_from_slice(&v.to_le_bytes());
    }

    pub(crate) fn w32(&mut self, a: u64, v: u32) {
        let o = self.arena_idx(a);
        self.arena[o..o + 4].copy_from_slice(&v.to_le_bytes());
    }

    pub(crate) fn w64(&mut self, a: u64, v: u64) {
        let o = self.arena_idx(a);
        self.arena[o..o + 8].copy_from_slice(&v.to_le_bytes());
    }
}

/// Reference synthesis state (ring buffers + ring pointer) driven exactly like `_mp3d_SubbandSynth`.
pub(crate) struct RefSynth {
    mem: Mem,
    vptr: u64,
}

impl RefSynth {
    pub(crate) fn new() -> RefSynth {
        RefSynth { mem: Mem::new(), vptr: ARENA_VA + V_OFF as u64 }
    }

    pub(crate) fn ring_slot(&self) -> usize {
        ((self.vptr >> 2) & 15) as usize
    }

    pub(crate) fn ring_bytes(&self) -> &[u8] {
        &self.mem.arena[V_OFF..V_OFF + 2 * V_CH_BYTES]
    }

    pub(crate) fn set_state(&mut self, slot: usize, ring: &[u8]) {
        self.mem.arena[V_OFF..V_OFF + 2 * V_CH_BYTES].copy_from_slice(ring);
        self.vptr = ARENA_VA + V_OFF as u64 + 4 * slot as u64;
    }

    /// `_mp3d_SubbandSynth(work, &S+8, nslots, stereo)` on the first 0x480 ints of `work` (in-place PCM halfwords).
    pub(crate) fn subband_synth(&mut self, work: &mut [i32], nslots: u32, stereo: bool) {
        for (i, w) in work[..IN_BYTES / 4].iter().enumerate() {
            let o = IN_OFF + 4 * i;
            self.mem.arena[o..o + 4].copy_from_slice(&(*w as u32).to_le_bytes());
        }
        let inp = ARENA_VA + IN_OFF as u64;
        let mut r = [0u64; 32];
        r[0] = inp;
        r[1] = self.vptr;
        r[2] = nslots as u64;
        r[3] = inp;
        r[12] = DCT_CONST_VA;
        r[14] = STACK_TOP;
        machine::plonkas(&mut self.mem, &mut r);
        let old = self.vptr;
        self.vptr = r[4];
        if stereo {
            let mut r = [0u64; 32];
            r[1] = old + V_CH_BYTES as u64;
            r[3] = inp + 2;
            r[0] = inp + 0x900;
            r[2] = nslots as u64;
            r[12] = DCT_CONST_VA;
            r[14] = STACK_TOP;
            machine::plonkas(&mut self.mem, &mut r);
        }
        for (i, w) in work[..IN_BYTES / 4].iter_mut().enumerate() {
            let o = IN_OFF + 4 * i;
            *w = u32::from_le_bytes(self.mem.arena[o..o + 4].try_into().unwrap()) as i32;
        }
    }
}
