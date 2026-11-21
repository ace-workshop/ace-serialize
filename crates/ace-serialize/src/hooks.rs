//! 测试钩子：统计 bulk 路径与逐元素路径的调用次数（供 AC-T01 见证）。
//!
//! Counters are thread-local and exist only under `cfg(test)`, so parallel test
//! threads never observe each other's element or bulk calls. Non-test builds
//! compile the notifications to no-ops and carry no global mutable state.

#[cfg(test)]
extern crate std;

#[cfg(test)]
std::thread_local! {
    static BULK_OP_COUNT: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
    static ELEMENT_OP_COUNT: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
}

#[cfg(test)]
pub fn reset() {
    BULK_OP_COUNT.with(|c| c.set(0));
    ELEMENT_OP_COUNT.with(|c| c.set(0));
}
#[cfg(test)]
pub fn snapshot() -> (usize, usize) {
    (
        BULK_OP_COUNT.with(core::cell::Cell::get),
        ELEMENT_OP_COUNT.with(core::cell::Cell::get),
    )
}
#[cfg(test)]
pub fn note_bulk() {
    BULK_OP_COUNT.with(|c| c.set(c.get() + 1));
}
#[cfg(test)]
pub fn note_element() {
    ELEMENT_OP_COUNT.with(|c| c.set(c.get() + 1));
}

#[cfg(not(test))]
#[inline(always)]
pub fn note_bulk() {}
#[cfg(not(test))]
#[inline(always)]
pub fn note_element() {}
