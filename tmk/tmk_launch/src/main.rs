//! TODO

#![allow(unsafe_code)]

fn main() {
    let initial_cr0: u64;
    unsafe {
        std::arch::asm! {
            "mov {r}, cr0",
            r = out(reg) initial_cr0,
        }
    }
    println!("\rInitial CR0: {:#x}", initial_cr0);

    let new_cr0 = initial_cr0 & !0x10u64 | 0x40u64;
    println!("\rSetting CR0 to: {:#x}", new_cr0);

    let read_cr0: u64;
    unsafe {
        std::arch::asm! {
            "mov cr0, {r}",
            "mov {r}, cr0",
            r = inout(reg) new_cr0 => read_cr0,
        }
    }
    println!("\rRead CR0: {:#x}", read_cr0);
}
