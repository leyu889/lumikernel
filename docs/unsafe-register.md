# unsafe 清单（S6）

> 本文件由 `tools/unsafe-audit.py` 生成，不要手改。
> 重新生成：`python tools/unsafe-audit.py`

## 汇总

| 项 | 值 |
|---|---|
| 代码行（去空行/注释） | 23860 |
| 含 `unsafe` 的行 | 336 |
| 密度 | **1.41%**（上限 3.0%） |
| 缺 `// SAFETY:` 说明的块 | 46 |

按照约定，`unsafe` 只允许出现在三类边界上：
① 架构相关（端口/寄存器/上下文切换）；② MMIO/DMA；③ 已经由上层
证明过不变量的裸指针操作（页表、用户内存拷贝）。业务逻辑里出现
`unsafe` 一律视为设计错误。

## 缺说明的块（必须补齐）

- `crates/kheap/src/lib.rs:68` — `unsafe fn hdr_size(p: usize) -> usize {`
- `crates/kheap/src/lib.rs:73` — `unsafe fn hdr_prev(p: usize) -> usize {`
- `crates/kheap/src/lib.rs:78` — `unsafe fn hdr_used(p: usize) -> bool {`
- `crates/kheap/src/lib.rs:83` — `unsafe fn set_hdr(p: usize, prev: usize, size: usize) {`
- `crates/kheap/src/lib.rs:90` — `unsafe fn link_next(p: usize) -> usize {`
- `crates/kheap/src/lib.rs:95` — `unsafe fn link_prev(p: usize) -> usize {`
- `crates/kheap/src/lib.rs:212` — `let next = unsafe { link_next(cur) };`
- `crates/kheap/src/lib.rs:213` — `let prev = unsafe { link_prev(cur) };`
- `crates/kheap/src/lib.rs:268` — `let mut size = unsafe { hdr_size(h) };`
- `crates/kheap/src/lib.rs:294` — `unsafe {`
- `crates/kheap/src/lib.rs:405` — `if unsafe { hdr_prev(p) } != prev_size {`
- `crates/kheap/src/lib.rs:408` — `if unsafe { hdr_used(p) } {`
- `crates/kheap/src/lib.rs:433` — `if unsafe { link_prev(c) } != last {`
- `crates/kheap/src/lib.rs:538` — `unsafe impl GlobalAlloc for LockedHeap {`
- `crates/kheap/src/lib.rs:539` — `unsafe fn alloc(&self, layout: Layout) -> *mut u8 {`
- `crates/kheap/src/lib.rs:550` — `unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {`
- `crates/kpt/src/lib.rs:70` — `unsafe fn read_entry(mem: &impl PhysMem, table: u64, i: usize) -> u64 {`
- `crates/kpt/src/lib.rs:76` — `unsafe fn write_entry(mem: &impl PhysMem, table: u64, i: usize, v: u64) {`
- `crates/kpt/src/lib.rs:319` — `unsafe { write_entry(mem, self.root, i, e) };`
- `lumikernel/src/arch/gdt.rs:198` — `let base = unsafe { (*GDT.0.get()).as_ptr() as u64 };`
- `lumikernel/src/console.rs:15` — `unsafe fn outb(port: u16, value: u8) {`
- `lumikernel/src/console.rs:21` — `unsafe fn inb(port: u16) -> u8 {`
- `lumikernel/src/console.rs:29` — `unsafe {`
- `lumikernel/src/console.rs:42` — `unsafe {`
- `lumikernel/src/console.rs:67` — `unsafe {`
- `lumikernel/src/console.rs:85` — `unsafe {`
- `lumikernel/src/dev/ata.rs:74` — `unsafe fn outw(port: u16, v: u16) {`
- `lumikernel/src/dev/virtio.rs:345` — `let q = unsafe { QueueMem::at(qs.mem_phys) };`
- `lumikernel/src/intr/apic.rs:93` — `base: unsafe { LAPIC_BASE },`
- `lumikernel/src/intr/apic.rs:142` — `base: unsafe { IOAPIC_BASE },`
- `lumikernel/src/intr/frame.rs:100` — `pub unsafe fn outb(port: u16, value: u8) {`
- `lumikernel/src/intr/frame.rs:106` — `pub unsafe fn inb(port: u16) -> u8 {`
- `lumikernel/src/intr/frame.rs:115` — `pub unsafe fn inw(port: u16) -> u16 {`
- `lumikernel/src/main.rs:288` — `let (count, ptr) = unsafe { e820::table(table_addr) };`
- `lumikernel/src/main.rs:303` — `let src = unsafe { e820::boot_source(table_addr) };`
- `lumikernel/src/main.rs:306` — `let sys_off = unsafe { e820::part_lba(table_addr) };`
- `lumikernel/src/main.rs:745` — `let t = unsafe { read_volatile((a + size as u64 - 8) as *const u64) };`
- `lumikernel/src/mm/frames.rs:120` — `unsafe { core::slice::from_raw_parts_mut(BITMAP.0.get() as *mut u8, BITMAP_BYTES`
- `lumikernel/src/mm/frames.rs:164` — `unsafe { *GLOBAL.alloc.get() = Some(a) };`
- `lumikernel/src/mm/heap.rs:28` — `unsafe fn alloc(&self, layout: Layout) -> *mut u8 {`
- `lumikernel/src/mm/heap.rs:38` — `unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {`
- `lumikernel/src/mm/paging.rs:154` — `unsafe { asm!("mov {}, cr0", out(reg) v, options(nomem, nostack, preserves_flags`
- `lumikernel/src/mm/paging.rs:310` — `unsafe { asm!("mov {}, cr4", out(reg) v, options(nomem, nostack, preserves_flags`
- `lumikernel/src/mm/paging.rs:797` — `let via_high = unsafe { core::ptr::read_volatile(kva as *const u64) };`
- `lumikernel/src/task/mod.rs:446` — `.find(|i| unsafe { (*base)[*i].tid == tid && (*base)[*i].state != State::Free })`
- `lumikernel/src/task/mod.rs:449` — `unsafe {`

## 逐条清单

| 文件:行 | 代码 | 证明（SAFETY） |
|---|---|---|
| `crates/kheap/src/lib.rs:62` | `unsafe fn hdr_raw(p: usize) -> usize {` | 以下函数的 SAFETY 前提统一为：`p` 指向一段由 `Heap` 独占管理、 |
| `crates/kheap/src/lib.rs:68` | `unsafe fn hdr_size(p: usize) -> usize {` | **缺** |
| `crates/kheap/src/lib.rs:73` | `unsafe fn hdr_prev(p: usize) -> usize {` | **缺** |
| `crates/kheap/src/lib.rs:78` | `unsafe fn hdr_used(p: usize) -> bool {` | **缺** |
| `crates/kheap/src/lib.rs:83` | `unsafe fn set_hdr(p: usize, prev: usize, size: usize) {` | **缺** |
| `crates/kheap/src/lib.rs:90` | `unsafe fn link_next(p: usize) -> usize {` | **缺** |
| `crates/kheap/src/lib.rs:95` | `unsafe fn link_prev(p: usize) -> usize {` | **缺** |
| `crates/kheap/src/lib.rs:176` | `unsafe {` | SAFETY: start..end 完全落在调用方移交的切片内，长度 >= MIN_BLOCK，16 对齐。 |
| `crates/kheap/src/lib.rs:210` | `let bsize = unsafe { hdr_size(cur) };` | SAFETY: cur 来自空闲链表，链表中每个节点都是堆内合法块头。 |
| `crates/kheap/src/lib.rs:212` | `let next = unsafe { link_next(cur) };` | **缺** |
| `crates/kheap/src/lib.rs:213` | `let prev = unsafe { link_prev(cur) };` | **缺** |
| `crates/kheap/src/lib.rs:221` | `unsafe {` | SAFETY: nb 落在 cur 块内部（need + rest == bsize）， |
| `crates/kheap/src/lib.rs:238` | `unsafe { set_hdr(cur, hdr_prev(cur), bsize \| USED) };` | SAFETY: 整块给出去，块头仍然有效。 |
| `crates/kheap/src/lib.rs:245` | `cur = unsafe { link_next(cur) };` | SAFETY: cur 是空闲块，next 指针由 unlink 维护。 |
| `crates/kheap/src/lib.rs:265` | `if !unsafe { hdr_used(h) } {` | SAFETY: h 落在堆内且 16 对齐，读块头安全。 |
| `crates/kheap/src/lib.rs:268` | `let mut size = unsafe { hdr_size(h) };` | **缺** |
| `crates/kheap/src/lib.rs:275` | `unsafe {` | SAFETY: nxt 是 h 的下一个邻居；若它是空闲块，则必在空闲链表里。 |
| `crates/kheap/src/lib.rs:291` | `let psize = unsafe { hdr_prev(h) };` | SAFETY: prev_size 由 alloc 的切分逻辑与 dealloc 的合并逻辑共同维护。 |
| `crates/kheap/src/lib.rs:294` | `unsafe {` | **缺** |
| `crates/kheap/src/lib.rs:308` | `unsafe {` | SAFETY: h..h+size 在堆内，size 是 16 的倍数。 |
| `crates/kheap/src/lib.rs:323` | `unsafe {` | SAFETY: blk 是刚释放/切分出来的空闲块，payload 前 16 字节可写。 |
| `crates/kheap/src/lib.rs:338` | `unsafe { *(prev as *mut usize).add(2) = next };` | SAFETY: prev 是链表中 blk 的前驱，其 next 指针可写。 |
| `crates/kheap/src/lib.rs:344` | `unsafe { *(next as *mut usize).add(3) = prev };` | SAFETY: next 是链表中 blk 的后继，其 prev 指针可写。 |
| `crates/kheap/src/lib.rs:354` | `unsafe {` | SAFETY: c 来自空闲链表。 |
| `crates/kheap/src/lib.rs:371` | `c = unsafe { link_next(c) };` | SAFETY: c 来自空闲链表。 |
| `crates/kheap/src/lib.rs:398` | `let sz = unsafe { hdr_size(p) };` | SAFETY: p 在堆内，块头可读。 |
| `crates/kheap/src/lib.rs:405` | `if unsafe { hdr_prev(p) } != prev_size {` | **缺** |
| `crates/kheap/src/lib.rs:408` | `if unsafe { hdr_used(p) } {` | **缺** |
| `crates/kheap/src/lib.rs:430` | `if unsafe { hdr_used(c) } {` | SAFETY: c 在堆内。 |
| `crates/kheap/src/lib.rs:433` | `if unsafe { link_prev(c) } != last {` | **缺** |
| `crates/kheap/src/lib.rs:438` | `c = unsafe { link_next(c) };` | SAFETY: c 是空闲块，next 指针可读。 |
| `crates/kheap/src/lib.rs:475` | `unsafe impl Sync for LockedHeap {}` | SAFETY: 所有对 inner 的访问都在 lock 保护之下（见 acquire/release）。 |
| `crates/kheap/src/lib.rs:489` | `pub unsafe fn init(&self, region: &'static mut [u8]) -> bool {` | # Safety |
| `crates/kheap/src/lib.rs:502` | `let ready = unsafe { (*self.inner.get()).is_some() };` | SAFETY: 持有锁。 |
| `crates/kheap/src/lib.rs:510` | `let s = unsafe { (*self.inner.get()).as_ref().map(\|h\| h.stats()) };` | SAFETY: 持有锁。 |
| `crates/kheap/src/lib.rs:518` | `let r = unsafe { (*self.inner.get()).as_ref().map(\|h\| h.audit()) };` | SAFETY: 持有锁。 |
| `crates/kheap/src/lib.rs:538` | `unsafe impl GlobalAlloc for LockedHeap {` | **缺** |
| `crates/kheap/src/lib.rs:539` | `unsafe fn alloc(&self, layout: Layout) -> *mut u8 {` | **缺** |
| `crates/kheap/src/lib.rs:550` | `unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {` | **缺** |
| `crates/kheap/src/lib.rs:680` | `unsafe {` | SAFETY: 两块都是刚分配、互不重叠的可写区域。 |
| `crates/kheap/src/lib.rs:686` | `unsafe {` | SAFETY: 同上，i < 100 在块内。 |
| `crates/kheap/src/lib.rs:770` | `unsafe {` | SAFETY: 故意破坏本测试自己拥有的堆内存，之后只做审计不做分配。 |
| `crates/kheap/src/lib.rs:789` | `unsafe { ptr::write_bytes(p, (step & 0xFF) as u8, size) };` | SAFETY: 刚分配的 size 字节可写。 |
| `crates/kheap/src/lib.rs:796` | `unsafe {` | SAFETY: 校验自己刚写的标记。 |
| `crates/kheap/src/lib.rs:850` | `unsafe { assert!(lh.init(region(64 * 1024))) };` | SAFETY: 这段内存交给 lh 独占，测试结束随进程回收。 |
| `crates/kheap/src/lib.rs:853` | `unsafe {` | SAFETY: 符合 GlobalAlloc 契约。 |
| `crates/kpt/src/lib.rs:70` | `unsafe fn read_entry(mem: &impl PhysMem, table: u64, i: usize) -> u64 {` | **缺** |
| `crates/kpt/src/lib.rs:76` | `unsafe fn write_entry(mem: &impl PhysMem, table: u64, i: usize, v: u64) {` | **缺** |
| `crates/kpt/src/lib.rs:83` | `unsafe { core::ptr::write_bytes(mem.at(phys), 0, PAGE_SIZE) }` | SAFETY: phys 是刚分配、未被使用的 4 KiB 帧。 |
| `crates/kpt/src/lib.rs:89` | `if unsafe { read_entry(mem, table, i) } != 0 {` | SAFETY: table 是有效页表页。 |
| `crates/kpt/src/lib.rs:113` | `pub unsafe fn from_root(root: u64) -> Self {` | # Safety |
| `crates/kpt/src/lib.rs:145` | `let e = unsafe { read_entry(mem, table, i) };` | SAFETY: table 一路都是有效页表页。 |
| `crates/kpt/src/lib.rs:155` | `unsafe { write_entry(mem, table, i, ne) };` | SAFETY: 同上。 |
| `crates/kpt/src/lib.rs:163` | `unsafe { write_entry(mem, table, i, e \| US) };` | SAFETY: 同上。 |
| `crates/kpt/src/lib.rs:171` | `let old = unsafe { read_entry(mem, table, i) };` | SAFETY: table 是最后一级页表页。 |
| `crates/kpt/src/lib.rs:176` | `unsafe { write_entry(mem, table, i, (paddr & ADDR_MASK) \| flags.bits()) };` | SAFETY: 同上。 |
| `crates/kpt/src/lib.rs:209` | `let e = unsafe { read_entry(mem, table, index_of(vaddr, level)) };` | SAFETY: table 是有效页表页。 |
| `crates/kpt/src/lib.rs:224` | `let e = unsafe { read_entry(mem, table, index_of(vaddr, 0)) };` | SAFETY: table 是最后一级页表页。 |
| `crates/kpt/src/lib.rs:251` | `let e = unsafe { read_entry(mem, table, index_of(vaddr, level)) };` | SAFETY: table 是有效页表页。 |
| `crates/kpt/src/lib.rs:265` | `let e = unsafe { read_entry(mem, table, i) };` | SAFETY: table 是最后一级页表页。 |
| `crates/kpt/src/lib.rs:270` | `unsafe { write_entry(mem, table, i, 0) };` | SAFETY: 同上。 |
| `crates/kpt/src/lib.rs:281` | `unsafe { write_entry(mem, tables[k - 1], index_of(vaddr, level), 0) };` | SAFETY: tables[k-1] 是 tables[k] 的父表，entry 存在。 |
| `crates/kpt/src/lib.rs:294` | `let e = unsafe { read_entry(mem, table, index_of(vaddr, l)) };` | SAFETY: table 是有效页表页。 |
| `crates/kpt/src/lib.rs:318` | `let e = unsafe { read_entry(mem, src_root, i) };` | SAFETY: 两张表都是有效 PML4 页。 |
| `crates/kpt/src/lib.rs:319` | `unsafe { write_entry(mem, self.root, i, e) };` | **缺** |
| `crates/kpt/src/lib.rs:352` | `unsafe { self.base.add(phys as usize) }` | SAFETY: 上面已确认 phys 在本次分配的内存范围内。 |
| `crates/kpt/src/lib.rs:566` | `unsafe { write_entry(&f.mem, pd, slot, 0x200000 \| P \| RW \| PS) };` | SAFETY: 直接用 PhysMem 改测试自己的页表。 |
| `crates/kslab/src/lib.rs:136` | `unsafe { ptr::read_volatile(addr as *const u32) }` | SAFETY: 调用方保证 addr 位于已映射、可读、4 字节对齐的内存中。本 crate |
| `crates/kslab/src/lib.rs:144` | `unsafe { ptr::write_volatile(addr as *mut u32, val) }` | SAFETY: 同 `ld_u32`；另外调用方保证该地址归本 slab 独占（页来源契约）， |
| `crates/kslab/src/lib.rs:151` | `unsafe { ptr::read_volatile(addr as *const u64) }` | SAFETY: 同 `ld_u32`；偏移均为 8 的倍数，对齐成立。 |
| `crates/kslab/src/lib.rs:158` | `unsafe { ptr::write_volatile(addr as *mut u64, val) }` | SAFETY: 同 `ld_u32`；偏移均为 8 的倍数，对齐成立。 |
| `crates/kslab/src/lib.rs:1084` | `unsafe { ptr::write_volatile(base as *mut u32, 0xDEAD_BEEF) };` | SAFETY: base 是页来源交付过的、已映射的页基址，写 4 字节在其头部合法。 |
| `crates/kslab/src/lib.rs:1102` | `unsafe { ptr::write_volatile((base + OFF_FREE_COUNT) as *mut u32, fc + 1) };` | SAFETY: base 是已交付页的基址，OFF_FREE_COUNT 是页头内 4 字节字段。 |
| `crates/kslab/src/lib.rs:1108` | `unsafe { ptr::write_volatile((base + OFF_FREE_COUNT) as *mut u32, fc) };` | SAFETY: 同上，把字段写回原值。 |
| `lumikernel/src/arch/gdt.rs:73` | `unsafe impl Sync for TssCell {}` | SAFETY: 启动阶段单核写入；运行期只有 CPU 自己读（切栈时读 rsp0/ist）。 |
| `lumikernel/src/arch/gdt.rs:78` | `unsafe impl Sync for GdtCell {}` | SAFETY: 建好后只读（CPU 读，我们只在初始化时写）。 |
| `lumikernel/src/arch/gdt.rs:132` | `let (base, limit, rsp0, ist1) = unsafe {` | SAFETY: 启动阶段单核、中断关闭，以下全部是对本模块静态数据的独占写入。 |
| `lumikernel/src/arch/gdt.rs:170` | `unsafe {` | SAFETY: gdtr 指向本栈帧上的合法 10 字节操作数；GDT 已填好且是静态内存， |
| `lumikernel/src/arch/gdt.rs:198` | `let base = unsafe { (*GDT.0.get()).as_ptr() as u64 };` | **缺** |
| `lumikernel/src/arch/gdt.rs:204` | `unsafe { gdt_load(core::ptr::addr_of!(gdtr) as *const u8) };` | SAFETY: GDT 已初始化且是静态内存；栈由调用方保证有效。 |
| `lumikernel/src/arch/gdt.rs:218` | `unsafe {` | SAFETY: packed 结构体字段写入，单核，调用点在调度器（中断关闭）。 |
| `lumikernel/src/arch/gdt.rs:229` | `unsafe { (*TSS.0.get()).rsp0 }` | SAFETY: 单核，只读。 |
| `lumikernel/src/arch/gdt.rs:236` | `unsafe { asm!("sgdt [{}]", in(reg) core::ptr::addr_of_mut!(g), options(nostack)) };` | SAFETY: sgdt 向 10 字节 packed 结构写入。 |
| `lumikernel/src/arch/gdt.rs:244` | `unsafe { asm!("str {0:x}", out(reg) v, options(nomem, nostack, preserves_flags)) };` | SAFETY: str 只读，无副作用。 |
| `lumikernel/src/arch/smp.rs:57` | `unsafe impl Sync for CpusCell {}` | SAFETY: 写入只发生在 `start_aps`（串行唤醒，每项写完后用 Release 发布 NCPU）， |
| `lumikernel/src/arch/smp.rs:104` | `unsafe { &*CPUS.0.get() }` | SAFETY: 只读访问；每一项在 `NCPU` 发布之前就已写好（Release/Acquire 配对）。 |
| `lumikernel/src/arch/smp.rs:113` | `unsafe {` | SAFETY: 两个约定单元，经直接映射区读取。 |
| `lumikernel/src/arch/smp.rs:123` | `unsafe {` | SAFETY: 同上；只在串行启动 AP 时使用。 |
| `lumikernel/src/arch/smp.rs:228` | `unsafe {` | SAFETY: 启动阶段只有 BSP 在跑，独占写入。 |
| `lumikernel/src/arch/smp.rs:252` | `unsafe {` | SAFETY: 源是链接器圈定的 .ap_tramp 段；目标是约定的实模式落点 0x8000， |
| `lumikernel/src/arch/smp.rs:282` | `unsafe {` | SAFETY: 串行启动，此时只有 BSP 在写这一项。 |
| `lumikernel/src/arch/smp.rs:289` | `unsafe {` | SAFETY: 0x7000 是约定的参数块；每次只服务一个 AP（串行启动）。 |
| `lumikernel/src/arch/smp.rs:354` | `unsafe {` | SAFETY: sti 只影响本核的标志寄存器。 |
| `lumikernel/src/arch/smp.rs:365` | `unsafe {` | SAFETY: hlt 等下一个中断（定时器或 IPI）。 |
| `lumikernel/src/arch/sync.rs:28` | `unsafe impl<T: Send> Sync for IrqSpinlock<T> {}` | SAFETY: 互斥由 `locked` 保证；`T: Send` 才允许跨执行流共享。 |
| `lumikernel/src/arch/sync.rs:30` | `unsafe impl<T: Send> Send for IrqSpinlock<T> {}` | SAFETY: 同 `Sync`。 |
| `lumikernel/src/arch/sync.rs:63` | `pub unsafe fn get_unchecked(&self) -> &T {` | # Safety |
| `lumikernel/src/arch/sync.rs:77` | `unsafe { &*self.lock.data.get() }` | SAFETY: 持有守卫期间独占访问。 |
| `lumikernel/src/arch/sync.rs:84` | `unsafe { &mut *self.lock.data.get() }` | SAFETY: 同上。 |
| `lumikernel/src/arch/sync.rs:100` | `unsafe {` | SAFETY: pushfq/pop 只读 RFLAGS；cli 清 IF。两者都不碰内存。 |
| `lumikernel/src/arch/sync.rs:117` | `unsafe { core::arch::asm!("sti", options(nomem, nostack, preserves_flags)) };` | SAFETY: 只在原来开着时才重新开启，语义与 x86 的 sti 一致。 |
| `lumikernel/src/arch/syscall.rs:70` | `unsafe {` | SAFETY: rdmsr 只读 MSR，无内存副作用；msr 全是本模块的常量。 |
| `lumikernel/src/arch/syscall.rs:80` | `unsafe {` | SAFETY: 写的是本架构定义的四个 MSR，值由上面的常量拼出。 |
| `lumikernel/src/arch/syscall.rs:179` | `let f = unsafe { &mut *frame };` | SAFETY: 指针指向 `syscall_entry` 刚在任务内核栈上拼好的 176 字节帧， |
| `lumikernel/src/console.rs:15` | `unsafe fn outb(port: u16, value: u8) {` | **缺** |
| `lumikernel/src/console.rs:21` | `unsafe fn inb(port: u16) -> u8 {` | **缺** |
| `lumikernel/src/console.rs:29` | `unsafe {` | **缺** |
| `lumikernel/src/console.rs:42` | `unsafe {` | **缺** |
| `lumikernel/src/console.rs:67` | `unsafe {` | **缺** |
| `lumikernel/src/console.rs:85` | `unsafe {` | **缺** |
| `lumikernel/src/console.rs:121` | `unsafe {` | SAFETY: VGA_BASE 是直接映射区里的文本缓冲，80x25 个 u16 单元全部可写。 |
| `lumikernel/src/console.rs:164` | `unsafe impl Sync for CapBuf {}` | SAFETY: 每个字节写自己 fetch_add 出来的槽位，不会有两个执行流写同一格。 |
| `lumikernel/src/console.rs:181` | `unsafe { (*CAP.0.get())[i] = b };` | SAFETY: i 是本执行流独占的槽位。 |
| `lumikernel/src/console.rs:189` | `let buf: &[u8] = unsafe { core::slice::from_raw_parts(CAP.0.get() as *const u8, n) };` | SAFETY: 只读前 n 个已写入的字节；缓冲是静态内存，生命周期够长。 |
| `lumikernel/src/console.rs:205` | `let src: &[u8] = unsafe { core::slice::from_raw_parts(CAP.0.get() as *const u8, n) };` | SAFETY: 同上，只读已写入部分。 |
| `lumikernel/src/dev/ahci.rs:121` | `unsafe {` | SAFETY: 单核；磁盘操作由 fs 层的 `IrqSpinlock` 串行化。 |
| `lumikernel/src/dev/ahci.rs:132` | `unsafe {` | SAFETY: 同上。 |
| `lumikernel/src/dev/ahci.rs:152` | `unsafe { slot < 2 && SLOTS[slot].used }` | SAFETY: 单核；SLOTS 初始化后只读。 |
| `lumikernel/src/dev/ahci.rs:157` | `unsafe {` | SAFETY: 单核；SLOTS 只在 init 期间写。 |
| `lumikernel/src/dev/ahci.rs:176` | `unsafe { core::ptr::read_volatile((sx + off) as *const u32) }` | SAFETY: sx 是映射过的 MMIO 区间，off 都在端口寄存器范围内。 |
| `lumikernel/src/dev/ahci.rs:182` | `unsafe { core::ptr::write_volatile((sx + off) as *mut u32, v) }` | SAFETY: 同上，写的是设备寄存器。 |
| `lumikernel/src/dev/ahci.rs:261` | `unsafe { SLOTS[idx] = slot };` | SAFETY: 单核、初始化阶段独占写 SLOTS。 |
| `lumikernel/src/dev/ahci.rs:305` | `unsafe { core::ptr::write_bytes(paging::phys_to_virt(p) as *mut u8, 0, 4096) };` | SAFETY: 刚分配的独占帧，经直接映射区清零。 |
| `lumikernel/src/dev/ahci.rs:348` | `unsafe { core::ptr::read_volatile((slot.buf as *const u16).add(i)) as u64 }` | SAFETY: buf 是一整页，i 远小于 512 个字。 |
| `lumikernel/src/dev/ahci.rs:363` | `unsafe {` | SAFETY: ct 是一页命令表，偏移都在 0x80+16 以内。 |
| `lumikernel/src/dev/ahci.rs:383` | `unsafe {` | SAFETY: 命令表的 PRDT 从 0x80 开始，一条 16 字节。 |
| `lumikernel/src/dev/ahci.rs:398` | `unsafe {` | SAFETY: 命令列表一页，头 0 在 32 字节内。 |
| `lumikernel/src/dev/ahci.rs:440` | `unsafe {` | SAFETY: 单核；SLOTS 在 init 之后只读。 |
| `lumikernel/src/dev/ahci.rs:458` | `unsafe {` | SAFETY: DMA 缓冲是一整页，读 1024 字节安全。 |
| `lumikernel/src/dev/ahci.rs:471` | `unsafe {` | SAFETY: 同上，先拷进 DMA 缓冲。 |
| `lumikernel/src/dev/ahci.rs:501` | `unsafe {` | SAFETY: `buf` 是一整页，`off + 512 <= 4096`（count ≤ 8）。 |
| `lumikernel/src/dev/ahci.rs:516` | `unsafe { core::ptr::copy_nonoverlapping(s.buf as *const u8, out.as_mut_ptr(), 512) };` | SAFETY: 同上。 |
| `lumikernel/src/dev/ahci.rs:525` | `unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), s.buf as *mut u8, 512) };` | SAFETY: 缓冲一整页。 |
| `lumikernel/src/dev/ata.rs:47` | `unsafe {` | SAFETY: 只读状态口，无副作用。 |
| `lumikernel/src/dev/ata.rs:56` | `unsafe { inb(PORT_STATUS) }` | SAFETY: 只读状态口。 |
| `lumikernel/src/dev/ata.rs:74` | `unsafe fn outw(port: u16, v: u16) {` | **缺** |
| `lumikernel/src/dev/ata.rs:101` | `unsafe {` | SAFETY: 选盘 + 读状态，不涉及内存安全。 |
| `lumikernel/src/dev/ata.rs:115` | `unsafe {` | SAFETY: 端口序列，无内存安全影响。 |
| `lumikernel/src/dev/ata.rs:130` | `*w = unsafe { inw(PORT_DATA) };` | SAFETY: DRQ 已置位，数据口可读。 |
| `lumikernel/src/dev/ata.rs:162` | `unsafe {` | SAFETY: 端口序列，无内存安全影响。 |
| `lumikernel/src/dev/ata.rs:175` | `let e = unsafe { inb(PORT_ERR) };` | SAFETY: 读错误寄存器（读它同时清中断标志）。 |
| `lumikernel/src/dev/ata.rs:190` | `let w = unsafe { inw(PORT_DATA) };` | SAFETY: DRQ 已置位；256 个字 = 512 字节。 |
| `lumikernel/src/dev/ata.rs:212` | `unsafe {` | SAFETY: 端口序列，无内存安全影响。 |
| `lumikernel/src/dev/ata.rs:232` | `unsafe { outw(PORT_DATA, w) };` | SAFETY: DRQ 已置位，数据口可写。 |
| `lumikernel/src/dev/ata.rs:253` | `unsafe {` | SAFETY: 端口序列，无内存安全影响。 |
| `lumikernel/src/dev/e1000.rs:115` | `unsafe { core::ptr::read_volatile((base + reg * 4) as *const u32) }` | SAFETY: base 是 PCI BAR0 映射的 MMIO 区（pages::phys_to_virt）， |
| `lumikernel/src/dev/e1000.rs:120` | `unsafe {` | SAFETY: 同上。 |
| `lumikernel/src/dev/e1000.rs:238` | `unsafe {` | SAFETY: 刚分配的独占页，经直接映射区写。 |
| `lumikernel/src/dev/e1000.rs:255` | `unsafe {` | SAFETY: 描述符页独占，下标在环内。 |
| `lumikernel/src/dev/e1000.rs:377` | `unsafe {` | SAFETY: MMIO 区与描述符页都已映射。 |
| `lumikernel/src/dev/e1000.rs:432` | `unsafe {` | SAFETY: 描述符页/缓冲帧/MMIO 都已映射，锁内独占。 |
| `lumikernel/src/dev/pci.rs:78` | `unsafe {` | SAFETY: 同上。 |
| `lumikernel/src/dev/pci.rs:265` | `unsafe {` | SAFETY: 标准 PCI 命令寄存器写，语义固定。 |
| `lumikernel/src/dev/virtio.rs:100` | `unsafe {` | SAFETY: port 是 PCI BAR0 给我们的 IO 窗口。 |
| `lumikernel/src/dev/virtio.rs:108` | `unsafe {` | SAFETY: 同上。 |
| `lumikernel/src/dev/virtio.rs:116` | `unsafe {` | SAFETY: 同上。 |
| `lumikernel/src/dev/virtio.rs:124` | `unsafe {` | SAFETY: 同上。 |
| `lumikernel/src/dev/virtio.rs:190` | `unsafe {` | SAFETY: 队列内存独占且已清零；`rx_mem` 来自 alloc_queue。 |
| `lumikernel/src/dev/virtio.rs:232` | `unsafe {` | SAFETY: 同上（IO 端口）。 |
| `lumikernel/src/dev/virtio.rs:242` | `unsafe fn at(phys: u64) -> *mut QueueMem {` | # Safety |
| `lumikernel/src/dev/virtio.rs:255` | `unsafe {` | SAFETY: 刚分配的独占连续两页，整块清零。 |
| `lumikernel/src/dev/virtio.rs:264` | `unsafe {` | SAFETY: 刚分配独占页。 |
| `lumikernel/src/dev/virtio.rs:294` | `let q = unsafe { QueueMem::at(qs.mem_phys) };` | SAFETY: qs.mem_phys 来自 alloc_queue，是独占的队列页。 |
| `lumikernel/src/dev/virtio.rs:298` | `unsafe {` | SAFETY: 队列与缓冲都由本驱动独占；索引已取模。 |
| `lumikernel/src/dev/virtio.rs:345` | `let q = unsafe { QueueMem::at(qs.mem_phys) };` | **缺** |
| `lumikernel/src/dev/virtio.rs:348` | `unsafe {` | SAFETY: 队列内存独占；used 环由设备写、我们只读。 |
| `lumikernel/src/e820.rs:44` | `pub unsafe fn part_lba(addr: usize) -> u32 {` | # Safety |
| `lumikernel/src/e820.rs:52` | `pub unsafe fn boot_source(addr: usize) -> &'static str {` | # Safety |
| `lumikernel/src/e820.rs:64` | `pub unsafe fn table(addr: usize) -> (usize, *const E820Entry) {` | # Safety |
| `lumikernel/src/e820.rs:73` | `unsafe { *ptr.add(i) }` | SAFETY: 调用方传入的 ptr 指向 stage2 写入的连续数组，i 已由 table() 的条目数约束。 |
| `lumikernel/src/exec.rs:122` | `unsafe {` | SAFETY: 刚分配的独占帧，经直接映射区清零。 |
| `lumikernel/src/exec.rs:135` | `unsafe {` | SAFETY: 目标在本帧内，源在 data 内（kelf 已校验 p_offset+p_filesz 不越界）。 |
| `lumikernel/src/exec.rs:167` | `unsafe {` | SAFETY: 刚分配的独占帧。 |
| `lumikernel/src/fs/mod.rs:41` | `unsafe impl Sync for CacheData {}` | SAFETY: 只通过 `FS` 的 `IrqSpinlock` 访问（`CachedDisk` 只存在于 |
| `lumikernel/src/fs/mod.rs:67` | `unsafe {` | SAFETY: 索引在范围内；底层缓冲生命周期是整个内核运行期， |
| `lumikernel/src/fs/mod.rs:76` | `unsafe {` | SAFETY: 同上，写入位置由 idx 界定。 |
| `lumikernel/src/intr/acpi.rs:58` | `unsafe fn phys_slice<'a>(phys: u64, len: usize) -> &'a [u8] {` | # Safety |
| `lumikernel/src/intr/acpi.rs:69` | `let head = unsafe { phys_slice(p, 36) };` | SAFETY: 这段是 BIOS 区，恒等/直接映射都覆盖；每次只读 36 字节。 |
| `lumikernel/src/intr/acpi.rs:93` | `let head = unsafe { phys_slice(root_phys, 36) };` | SAFETY: RSDT 头固定 36 字节，是所有 ACPI 表的共同头。 |
| `lumikernel/src/intr/acpi.rs:99` | `let table = unsafe { phys_slice(root_phys, len) };` | SAFETY: 长度已判界。 |
| `lumikernel/src/intr/acpi.rs:111` | `let h = unsafe { phys_slice(addr, 36) };` | SAFETY: 指向另一张 ACPI 表的头，36 字节总是可读的。 |
| `lumikernel/src/intr/acpi.rs:126` | `let head = unsafe { phys_slice(madt_phys, 36) };` | SAFETY: 先读 36 字节头拿长度，再按长度取整张表。 |
| `lumikernel/src/intr/acpi.rs:132` | `let raw = unsafe { phys_slice(madt_phys, len) };` | SAFETY: 长度已判界。 |
| `lumikernel/src/intr/apic.rs:60` | `unsafe { read_volatile((self.base + off) as *const u32) }` | SAFETY: MMIO 页已映射且不可缓存；偏移都是 16 字节对齐的寄存器。 |
| `lumikernel/src/intr/apic.rs:65` | `unsafe { write_volatile((self.base + off) as *mut u32, v) }` | SAFETY: 同上。 |
| `lumikernel/src/intr/apic.rs:84` | `unsafe {` | SAFETY: rdtsc 只读计数器，无副作用。lfence 保证不乱序。 |
| `lumikernel/src/intr/apic.rs:93` | `base: unsafe { LAPIC_BASE },` | **缺** |
| `lumikernel/src/intr/apic.rs:116` | `unsafe {` | SAFETY: IOREGSEL/IOWIN 是标准两寄存器窗口。 |
| `lumikernel/src/intr/apic.rs:123` | `unsafe {` | SAFETY: 同上。 |
| `lumikernel/src/intr/apic.rs:137` | `unsafe { GSI_BASE = v };` | SAFETY: 启动阶段单核写常量。 |
| `lumikernel/src/intr/apic.rs:142` | `base: unsafe { IOAPIC_BASE },` | **缺** |
| `lumikernel/src/intr/apic.rs:149` | `let base_gsi = unsafe { GSI_BASE };` | SAFETY: 启动阶段已写入，之后只读。 |
| `lumikernel/src/intr/apic.rs:196` | `unsafe { LAPIC_BASE = LAPIC_VA };` | SAFETY: 单核启动阶段写这个全局。 |
| `lumikernel/src/intr/apic.rs:202` | `unsafe {` | SAFETY: rdmsr/wrmsr 是特权指令，参数固定。 |
| `lumikernel/src/intr/apic.rs:208` | `unsafe {` | SAFETY: 同上。 |
| `lumikernel/src/intr/apic.rs:231` | `unsafe { IOAPIC_BASE = IOAPIC_VA };` | SAFETY: 同上。 |
| `lumikernel/src/intr/frame.rs:55` | `unsafe { asm!("mov {}, cr2", out(reg) v, options(nomem, nostack, preserves_flags)) };` | SAFETY: 读 CR2 只返回最近一次缺页的线性地址，无副作用。 |
| `lumikernel/src/intr/frame.rs:64` | `unsafe { asm!("sti", options(nomem, nostack, preserves_flags)) };` | SAFETY: sti 之后中断可能立刻到来，调用方必须保证 IDT/栈已就绪。 |
| `lumikernel/src/intr/frame.rs:70` | `unsafe { asm!("cli", options(nomem, nostack, preserves_flags)) };` | SAFETY: cli 只清 IF，无内存副作用。 |
| `lumikernel/src/intr/frame.rs:78` | `unsafe { asm!("pushfq", "pop {}", out(reg) f, options(nomem, preserves_flags)) };` | SAFETY: pushfq 只把 RFLAGS 压栈再弹回，不改状态。 |
| `lumikernel/src/intr/frame.rs:86` | `unsafe { asm!("hlt", options(nomem, nostack, preserves_flags)) };` | SAFETY: hlt 是特权指令，ring0 可用；不会改内存。 |
| `lumikernel/src/intr/frame.rs:93` | `unsafe { asm!("cli; hlt", options(nomem, nostack, preserves_flags)) };` | SAFETY: 同 halt_once。 |
| `lumikernel/src/intr/frame.rs:100` | `pub unsafe fn outb(port: u16, value: u8) {` | **缺** |
| `lumikernel/src/intr/frame.rs:106` | `pub unsafe fn inb(port: u16) -> u8 {` | **缺** |
| `lumikernel/src/intr/frame.rs:115` | `pub unsafe fn inw(port: u16) -> u16 {` | **缺** |
| `lumikernel/src/intr/handler.rs:39` | `let f = unsafe { &mut *frame };` | SAFETY: 指针来自 `isr_common` 压好的栈帧，布局与 `InterruptFrame` 一致 |
| `lumikernel/src/intr/handler.rs:194` | `let e = unsafe { core::ptr::read_volatile(va as *const u64) };` | SAFETY: 上面确认过这个直接映射地址可读。 |
| `lumikernel/src/intr/handler.rs:234` | `let v = unsafe {` | SAFETY: translate_in 已确认这一页映射在 CR3 指向的地址空间里。 |
| `lumikernel/src/intr/idt.rs:85` | `unsafe impl Sync for IdtCell {}` | SAFETY: 只在单核启动阶段由 `init` 写入一次；装载后 CPU 只读。 |
| `lumikernel/src/intr/idt.rs:116` | `let base = unsafe {` | SAFETY: 启动阶段单核、中断关闭，`IDT` 无并发访问者； |
| `lumikernel/src/intr/idt.rs:148` | `unsafe { asm!("lidt [{}]", in(reg) p, options(readonly, nostack, preserves_flags)) };` | SAFETY: idtr 是 10 字节的合法操作数，指向本函数栈上的局部变量， |
| `lumikernel/src/intr/idt.rs:165` | `unsafe { asm!("lidt [{}]", in(reg) p, options(readonly, nostack, preserves_flags)) };` | SAFETY: base 是本模块建好的 IDT；idtr 是本栈帧上的合法 10 字节操作数。 |
| `lumikernel/src/intr/idt.rs:174` | `unsafe {` | SAFETY: IDT 已装载；中断关闭期间改写单个门不会有并发读取。 |
| `lumikernel/src/intr/idt.rs:193` | `unsafe { asm!("sidt [{}]", in(reg) p, options(nostack, preserves_flags)) };` | SAFETY: sidt 向指向的 10 字节写入，局部变量足够大且对齐（packed）。 |
| `lumikernel/src/intr/idt.rs:203` | `Some(unsafe { *(*IDT.0.get()).get(vector as usize)? })` | SAFETY: `IDT` 装载后只读；下标已判界。 |
| `lumikernel/src/intr/pic.rs:28` | `unsafe { outb(0x80, 0) };` | SAFETY: 0x80 是 POST 诊断端口，写任意值无副作用。 |
| `lumikernel/src/intr/pic.rs:35` | `unsafe {` | SAFETY: 以下全部是对 8259A 标准端口的初始化序列（ICW1..ICW4）， |
| `lumikernel/src/intr/pic.rs:67` | `unsafe {` | SAFETY: 写 IMR 是 PIC 的标准操作。 |
| `lumikernel/src/intr/pic.rs:76` | `unsafe { (inb(PIC1_DATA), inb(PIC2_DATA)) }` | SAFETY: 读 IMR 只是读端口。 |
| `lumikernel/src/intr/pic.rs:85` | `unsafe {` | SAFETY: 往命令口写 0x20 是标准 EOI。 |
| `lumikernel/src/intr/pit.rs:35` | `unsafe {` | SAFETY: 写 PIT 命令口/数据口，不涉及任何内存安全。 |
| `lumikernel/src/intr/probe.rs:38` | `unsafe impl Sync for CtxCell {}` | SAFETY: 单核、中断关闭期间使用；处理器与发起方是同一执行流， |
| `lumikernel/src/intr/probe.rs:81` | `let a = unsafe { *action };` | SAFETY: 指针来自 `run` 里的栈上局部变量，在 `__probe` 返回前一直有效。 |
| `lumikernel/src/intr/probe.rs:87` | `unsafe {` | SAFETY: 故意让 `div` 除零触发 #DE；不会返回， |
| `lumikernel/src/intr/probe.rs:102` | `unsafe { core::ptr::read_volatile(p as *const u64) }` | SAFETY: 故意读可能未映射的地址；由探针机制兜底。 |
| `lumikernel/src/intr/probe.rs:106` | `unsafe { core::ptr::write_volatile(p as *mut u64, v) };` | SAFETY: 同 Read。 |
| `lumikernel/src/intr/probe.rs:111` | `unsafe { asm!("int3", options(nostack)) };` | SAFETY: 单字节 int3，处理器会推进 rip 后返回。 |
| `lumikernel/src/intr/probe.rs:136` | `let ctx = unsafe { &mut *CTX.0.get() };` | SAFETY: 单核执行；`CTX` 在调用期间独占。 |
| `lumikernel/src/intr/probe.rs:143` | `let r = unsafe { __probe(cp, ap) };` | SAFETY: 两个指针都指向本栈帧上的有效对象，生命周期覆盖整个调用。 |
| `lumikernel/src/intr/probe.rs:155` | `let ctx = unsafe { &mut *CTX.0.get() };` | SAFETY: 与 `run` 同一执行流，且此时 `run` 尚未返回（栈上有它的帧）。 |
| `lumikernel/src/intr/rtc.rs:17` | `unsafe {` | SAFETY: 0x70 是索引口、0x71 是数据口；读 CMOS 无副作用。 |
| `lumikernel/src/linux/mem.rs:89` | `unsafe { core::ptr::write_bytes(paging::phys_to_virt(phys) as *mut u8, 0, PAGE as usize) };` | SAFETY: 刚分配的独占帧，经直接映射区清零。 |
| `lumikernel/src/linux/mem.rs:194` | `unsafe { core::ptr::write_bytes(paging::phys_to_virt(phys) as *mut u8, 0, PAGE as usize) };` | SAFETY: 刚分配的独占帧。 |
| `lumikernel/src/linux/mem.rs:281` | `unsafe {` | SAFETY: 两个区间都在当前进程的地址空间里且已映射；系统调用期间 |
| `lumikernel/src/linux/proc.rs:421` | `unsafe {` | SAFETY: rdtsc 只读时间戳计数器。 |
| `lumikernel/src/linux/trace.rs:40` | `unsafe impl Sync for TraceCell {}` | SAFETY: 单核调度；索引用原子自增，每个执行流独占自己的槽位。 |
| `lumikernel/src/linux/trace.rs:50` | `unsafe {` | SAFETY: i 是本执行流独占的槽位（fetch_add 保证不重复）。 |
| `lumikernel/src/linux/trace.rs:81` | `let e = unsafe { (*TRACE.0.get())[i] };` | SAFETY: 只读已写入的槽位。 |
| `lumikernel/src/linux/uaccess.rs:105` | `*slot = unsafe { core::ptr::read_volatile((uptr + i as u64) as *const u8) };` | SAFETY: 上面逐页确认可读，且抢占已关。 |
| `lumikernel/src/linux/uaccess.rs:126` | `unsafe { core::ptr::write_volatile((uptr + i as u64) as *mut u8, *b) };` | SAFETY: 已确认可写，抢占已关。 |
| `lumikernel/src/linux/uaccess.rs:149` | `let b = unsafe { core::ptr::read_volatile((uptr + i as u64) as *const u8) };` | SAFETY: 刚确认这个字节可读。 |
| `lumikernel/src/main.rs:153` | `unsafe {` | SAFETY: 同 `paint_stack`，只读栈的未使用区间。 |
| `lumikernel/src/main.rs:170` | `unsafe {` | SAFETY: 金丝雀是我们自己画的一页静态内存。 |
| `lumikernel/src/main.rs:288` | `let (count, ptr) = unsafe { e820::table(table_addr) };` | **缺** |
| `lumikernel/src/main.rs:303` | `let src = unsafe { e820::boot_source(table_addr) };` | **缺** |
| `lumikernel/src/main.rs:306` | `let sys_off = unsafe { e820::part_lba(table_addr) };` | **缺** |
| `lumikernel/src/main.rs:661` | `let pat = unsafe {` | SAFETY: 这一页刚建立映射且归本测试独占。 |
| `lumikernel/src/main.rs:732` | `unsafe {` | SAFETY: slab 返回的地址属于本测试，长度 >= 16。 |
| `lumikernel/src/main.rs:744` | `let h = unsafe { read_volatile(a as *const u64) };` | SAFETY: 同上，仍在持有期内。 |
| `lumikernel/src/main.rs:745` | `let t = unsafe { read_volatile((a + size as u64 - 8) as *const u64) };` | **缺** |
| `lumikernel/src/main.rs:2376` | `let mut f: intr::frame::InterruptFrame = unsafe { core::mem::zeroed() };` | SAFETY: `InterruptFrame` 全是 u64，零值合法；这里只是借用它的 |
| `lumikernel/src/main.rs:3050` | `unsafe {` | SAFETY: 故意访问未映射地址。PML4[257] 整段没有映射。 |
| `lumikernel/src/mm/fault.rs:217` | `unsafe {` | SAFETY: `old` 是当前地址空间里已映射的可读页，`newp` 是刚分配独占的页； |
| `lumikernel/src/mm/fault.rs:271` | `unsafe {` | SAFETY: `phys` 是刚分配独占的帧，经直接映射区可写。 |
| `lumikernel/src/mm/fault.rs:277` | `let mut pt = unsafe { PageTable::from_root(pml4) };` | SAFETY: `pml4` 是当前 CR3 指向的根，`from_root` 只借用不拥有， |
| `lumikernel/src/mm/frames.rs:27` | `unsafe impl Sync for Bitmap {}` | SAFETY: 位图只在 `GLOBAL` 的锁保护下通过 `&mut` 访问。 |
| `lumikernel/src/mm/frames.rs:37` | `unsafe impl Sync for Global {}` | SAFETY: 同上，`alloc` 只在持锁期间访问。 |
| `lumikernel/src/mm/frames.rs:88` | `let (s, e) = unsafe {` | SAFETY: __kernel_start / __bss_end 由 linker.ld 定义为内核映像的 |
| `lumikernel/src/mm/frames.rs:120` | `unsafe { core::slice::from_raw_parts_mut(BITMAP.0.get() as *mut u8, BITMAP_BYTES) };` | **缺** |
| `lumikernel/src/mm/frames.rs:164` | `unsafe { *GLOBAL.alloc.get() = Some(a) };` | **缺** |
| `lumikernel/src/mm/frames.rs:189` | `let r = unsafe { (*GLOBAL.alloc.get()).as_mut().map(f) };` | SAFETY: 持锁期间独占访问分配器。 |
| `lumikernel/src/mm/heap.rs:27` | `unsafe impl GlobalAlloc for PreemptSafe {` | SAFETY: 只是把调用转给 HEAP，额外做的只有一对原子加减。 |
| `lumikernel/src/mm/heap.rs:28` | `unsafe fn alloc(&self, layout: Layout) -> *mut u8 {` | **缺** |
| `lumikernel/src/mm/heap.rs:38` | `unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {` | **缺** |
| `lumikernel/src/mm/heap.rs:53` | `let region = unsafe { core::slice::from_raw_parts_mut(va as *mut u8, HEAP_BYTES) };` | SAFETY: 这段物理内存刚从帧分配器摘走，之前没有任何代码持有它； |
| `lumikernel/src/mm/heap.rs:55` | `if !(unsafe { HEAP.init(region) }) {` | SAFETY: 同上，region 独占。 |
| `lumikernel/src/mm/paging.rs:95` | `unsafe { asm!("mov {}, cr3", out(reg) v, options(nomem, nostack, preserves_flags)) };` | SAFETY: 读控制寄存器无副作用。 |
| `lumikernel/src/mm/paging.rs:101` | `unsafe { asm!("mov cr3, {}", in(reg) v, options(nostack)) };` | SAFETY: 调用方保证 v 是有效 PML4 物理地址，且新表映射了当前代码与栈。 |
| `lumikernel/src/mm/paging.rs:107` | `unsafe { asm!("lea {}, [rip]", out(reg) v, options(nomem, nostack, preserves_flags)) };` | SAFETY: 读指令指针无副作用。 |
| `lumikernel/src/mm/paging.rs:118` | `unsafe { asm!("invlpg [{}]", in(reg) vaddr, options(nostack, preserves_flags)) };` | SAFETY: invlpg 只影响 TLB，不访问内存。 |
| `lumikernel/src/mm/paging.rs:124` | `let mut pt = unsafe { PageTable::from_root(read_cr3()) };` | SAFETY: 当前 CR3 一定是有效 PML4；我们是内核，有权改自己的地址空间。 |
| `lumikernel/src/mm/paging.rs:146` | `let mut pt = unsafe { PageTable::from_root(read_cr3()) };` | SAFETY: 同 map_in_current。 |
| `lumikernel/src/mm/paging.rs:154` | `unsafe { asm!("mov {}, cr0", out(reg) v, options(nomem, nostack, preserves_flags)) };` | **缺** |
| `lumikernel/src/mm/paging.rs:160` | `unsafe {` | SAFETY: 调用方保证只置 CPUID 报支持的位。 |
| `lumikernel/src/mm/paging.rs:171` | `unsafe {` | SAFETY: cpuid 无副作用；CPUID 在所有 x86_64 上可用。 |
| `lumikernel/src/mm/paging.rs:310` | `unsafe { asm!("mov {}, cr4", out(reg) v, options(nomem, nostack, preserves_flags)) };` | **缺** |
| `lumikernel/src/mm/paging.rs:319` | `unsafe { core::ptr::read_volatile((phys_to_virt(table_phys) as *const u64).add(idx)) }` | SAFETY: 调用方只传本进程页表里已验证 present 的表页物理地址； |
| `lumikernel/src/mm/paging.rs:494` | `unsafe {` | SAFETY: 两个物理帧都独占（src 属于调用者，newp 刚分配）， |
| `lumikernel/src/mm/paging.rs:560` | `Some(unsafe { core::ptr::read_volatile(phys_to_virt(slot) as *const u64) })` | SAFETY: `pte_slot` 已逐级验证过这张表页存在，直接映射区可读。 |
| `lumikernel/src/mm/paging.rs:576` | `unsafe {` | SAFETY: 同上。 |
| `lumikernel/src/mm/paging.rs:601` | `unsafe {` | SAFETY: `pte_slot` 已逐级验证过这条链路存在，直接映射区可写。 |
| `lumikernel/src/mm/paging.rs:666` | `unsafe {` | SAFETY: phys 是当前地址空间里这一页的物理地址，n 不越过页边界。 |
| `lumikernel/src/mm/paging.rs:687` | `unsafe {` | SAFETY: phys 是这一页的物理地址，n 不越页。 |
| `lumikernel/src/mm/paging.rs:796` | `let via_direct = unsafe { core::ptr::read_volatile(phys_to_virt(kstart) as *const u64) };` | SAFETY: 内核映像首 8 字节一定可读，两个地址都指向同一物理页。 |
| `lumikernel/src/mm/paging.rs:797` | `let via_high = unsafe { core::ptr::read_volatile(kva as *const u64) };` | **缺** |
| `lumikernel/src/mm/paging.rs:804` | `let current = unsafe { PageTable::from_root(old_root) };` | SAFETY: old_root 来自 CR3，一定是有效 PML4。 |
| `lumikernel/src/mm/paging.rs:824` | `unsafe { core::ptr::write_bytes(phys_to_virt(data) as *mut u8, 0, PAGE_SIZE) };` | SAFETY: data 是刚分配、当前无人使用的帧，通过直接映射访问。 |
| `lumikernel/src/mm/paging.rs:829` | `let observed = unsafe {` | SAFETY: 新表已复制内核高半区（含当前代码与栈）并映射了 VMALLOC_BASE。 |
| `lumikernel/src/mm/paging.rs:839` | `let via_direct_again = unsafe { core::ptr::read_volatile(phys_to_virt(data) as *const u64) };` | SAFETY: 同一个物理帧，走直接映射读。 |
| `lumikernel/src/syscall.rs:177` | `let old = unsafe { core::ptr::read_volatile(page as *const u8) };` | SAFETY: 调用点已先用 user_range_ok 确认整段可读。 |
| `lumikernel/src/syscall.rs:183` | `unsafe { core::ptr::write_volatile(page as *mut u8, old) };` | SAFETY: 刚才那次写成功了，这一页确实可写。 |
| `lumikernel/src/syscall.rs:206` | `out[i] = unsafe { core::ptr::read_volatile((ptr + i as u64) as *const u8) };` | SAFETY: 上面逐页确认过可读，且抢占已关。 |
| `lumikernel/src/syscall.rs:261` | `unsafe { core::ptr::write_volatile((ptr + i as u64) as *mut u8, buf[i]) }` | SAFETY: 上面已确认这段用户内存可写；抢占已关，期间不会被换出。 |
| `lumikernel/src/task/mod.rs:154` | `unsafe impl Sync for TableCell {}` | SAFETY: 单核，所有访问都在中断关闭或 `preempt_disable` 区间内。 |
| `lumikernel/src/task/mod.rs:165` | `unsafe impl Sync for SchedCell {}` | SAFETY: 同 `TASKS`。 |
| `lumikernel/src/task/mod.rs:172` | `unsafe { (*SCHED.0.get()).assume_init_mut() }` | SAFETY: `init` 已用 `init_in_place` 把每个字段写了一遍； |
| `lumikernel/src/task/mod.rs:270` | `unsafe {` | SAFETY: 启动阶段单核、中断关闭；对任务表独占写入。 |
| `lumikernel/src/task/mod.rs:320` | `unsafe {` | SAFETY: 这段内存是刚分配、当前无人使用的内核栈；下面写入的字段全部有明确含义。 |
| `lumikernel/src/task/mod.rs:344` | `let t = unsafe { &mut *TASKS.0.get() };` | SAFETY: 已持锁独占。 |
| `lumikernel/src/task/mod.rs:381` | `unsafe {` | SAFETY: slot 刚分配独占；启动阶段 AP 尚未接收任务。 |
| `lumikernel/src/task/mod.rs:426` | `unsafe { core::arch::asm!("hlt", options(nomem, nostack, preserves_flags)) };` | SAFETY: 没有可跑任务时等中断（定时器 tick 或未来的 IPI）。 |
| `lumikernel/src/task/mod.rs:446` | `.find(\|i\| unsafe { (*base)[*i].tid == tid && (*base)[*i].state != State::Free });` | **缺** |
| `lumikernel/src/task/mod.rs:449` | `unsafe {` | **缺** |
| `lumikernel/src/task/mod.rs:497` | `unsafe {` | SAFETY: 单核 + 抢占已关闭；slot 与 tid 都是本函数独占。 |
| `lumikernel/src/task/mod.rs:554` | `unsafe {` | SAFETY: code_phys 是刚摘下的帧；通过直接映射区拷入用户映像（长度已判界）。 |
| `lumikernel/src/task/mod.rs:566` | `unsafe { core::ptr::write_bytes(paging::phys_to_virt(stack_phys) as *mut u8, 0, 4096) };` | SAFETY: 同上，栈页清零即可。 |
| `lumikernel/src/task/mod.rs:604` | `unsafe {` | SAFETY: 同 spawn_kthread。 |
| `lumikernel/src/task/mod.rs:648` | `unsafe {` | SAFETY: 单核 + 抢占已关闭；slot 由本函数独占。 |
| `lumikernel/src/task/mod.rs:685` | `unsafe {` | SAFETY: frame_addr 在本函数独占的新内核栈内；src 是当前栈上的合法帧。 |
| `lumikernel/src/task/mod.rs:723` | `unsafe {` | SAFETY: 单核，只读。 |
| `lumikernel/src/task/mod.rs:734` | `unsafe { (*TASKS.0.get()).get(current_index()).map(\|t\| t.tid).unwrap_or(0) }` | SAFETY: 单核，读单个字段。 |
| `lumikernel/src/task/mod.rs:747` | `unsafe {` | SAFETY: 单核 + 中断关闭，独占访问任务表与调度器。 |
| `lumikernel/src/task/mod.rs:820` | `unsafe {` | SAFETY: 单核；只给当前任务累加 vruntime。 |
| `lumikernel/src/task/mod.rs:840` | `unsafe {` | SAFETY: 单核；这里只读任务状态与累加计数。 |
| `lumikernel/src/task/mod.rs:864` | `let tid_of = \|_c: usize\| -> u64 { unsafe { (*TASKS.0.get())[_c].tid } };` | SAFETY: 单核；读自己的 tid（下面要拿它当"被等的对象"）。 |
| `lumikernel/src/task/mod.rs:866` | `unsafe {` | SAFETY: 单核；下面是自己的收尾。 |
| `lumikernel/src/task/mod.rs:915` | `unsafe {` | SAFETY: 见上；任务表是静态内存，生命周期够长。 |
| `lumikernel/src/task/mod.rs:936` | `unsafe {` | SAFETY: 只读遍历；tid 对应的槽位由任务生命周期保证存在。 |
| `lumikernel/src/task/mod.rs:952` | `unsafe {` | SAFETY: 单核 + 中断关闭；下面是自己的状态迁移。 |
| `lumikernel/src/task/mod.rs:973` | `unsafe {` | SAFETY: 同上。 |
| `lumikernel/src/task/mod.rs:992` | `unsafe {` | SAFETY: 单核；只改目标任务的状态并把它塞回运行队列。 |
| `lumikernel/src/task/mod.rs:1022` | `unsafe impl Sync for ExitWaitCell {}` | SAFETY: 单核；只在关中断区间内读写（`wait_zombie` / `exit_current`）。 |
| `lumikernel/src/task/mod.rs:1027` | `unsafe {` | SAFETY: 单核 + 中断关闭（调用方 `wait_zombie` 保证）。 |
| `lumikernel/src/task/mod.rs:1041` | `unsafe {` | SAFETY: 同上。 |
| `lumikernel/src/task/mod.rs:1060` | `unsafe {` | SAFETY: 单核，只读。 |
| `lumikernel/src/task/mod.rs:1130` | `unsafe {` | SAFETY: 单核，只读。 |
| `lumikernel/src/task/mod.rs:1145` | `unsafe {` | SAFETY: 单核，只读。 |
| `lumikernel/src/task/mod.rs:1217` | `let (n, list) = unsafe {` | SAFETY: 中断当前是关的（上面 disable 过），独占任务表。 |
| `lumikernel/src/task/mod.rs:1252` | `unsafe {` | SAFETY: 单核 + 中断关闭；只动目标任务（它已不运行）。 |
| `lumikernel/src/task/mod.rs:1322` | `unsafe {` | SAFETY: 单核，只读。 |
| `lumikernel/src/task/mod.rs:1336` | `unsafe { (*TASKS.0.get()).iter().filter(\|t\| t.state != State::Free).count() }` | SAFETY: 单核，只读。 |
| `lumikernel/src/task/mod.rs:1341` | `unsafe { (*TASKS.0.get()).iter().filter(\|t\| t.state == state).count() }` | SAFETY: 单核，只读。 |
| `lumikernel/src/task/mod.rs:1514` | `unsafe {` | SAFETY: 单核，只读。 |
| `lumikernel/src/task/mod.rs:1531` | `unsafe {` | SAFETY: 单核；目标任务还没被调度，不会被并发访问。 |
| `lumikernel/src/task/mod.rs:1553` | `unsafe {` | SAFETY: 单核，只读。 |
| `lumikernel/src/task/mod.rs:1565` | `unsafe {` | SAFETY: 单核，只读。 |
